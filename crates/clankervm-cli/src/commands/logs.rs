use crate::client::{LogEvent, LogPage, LogQuery, LogWindow, MicroVmClient};
use crate::config::{ProjectConfig, Settings};
use crate::output::render;
use crate::util::validate_non_empty;
use crate::{ClankerError, OutputFormat};
use aws_smithy_types::DateTime;
use clap::Args;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::io::Write;
use std::pin::pin;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

/// The log group AWS streams MicroVM logs to by default.
const DEFAULT_GROUP_PREFIX: &str = "/aws/lambda-microvms";
/// Events requested per read when `--limit` is not configured.
const DEFAULT_LIMIT: usize = 1000;
/// Events one read may ask for.
const MAX_LIMIT: usize = 10_000;
/// How long `--follow` waits before asking for more events.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Args)]
pub struct LogsOptions {
    /// MicroVM to read logs for, as reported by run or list.
    #[arg(value_name = "MICROVM_ID", value_parser = parse_microvm_id)]
    pub microvm_id: String,
    /// Keep printing new events until interrupted.
    #[arg(long)]
    pub follow: bool,
    /// Print event messages without their timestamps.
    #[arg(long)]
    pub raw: bool,
    #[command(flatten)]
    pub settings: LogsSettings,
}

/// Logs settings, shared by `--flags` and the `[microvm.run.logs]` table.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct LogsSettings {
    /// Log group to read; defaults to the group run streams to.
    #[arg(long)]
    #[serde(rename = "group")]
    pub log_group: Option<String>,
    /// Read events from this long before now, for example 30m.
    #[arg(long, value_parser = humantime::parse_duration)]
    #[serde(with = "humantime_serde")]
    pub since: Option<Duration>,
    /// Events to report per read; defaults to 1000, at most 10000.
    #[arg(long)]
    pub limit: Option<usize>,
}

impl Settings for LogsSettings {
    fn validate(&self) -> Result<(), ClankerError> {
        validate_non_empty(self.log_group.as_deref(), "microvm.run.logs.group")?;
        if self
            .limit
            .is_some_and(|limit| !(1..=MAX_LIMIT).contains(&limit))
        {
            return Err(ClankerError::InvalidConfig(format!(
                "microvm.run.logs.limit must be between 1 and {MAX_LIMIT}"
            )));
        }
        Ok(())
    }
}

impl LogsSettings {
    /// The shared run log group or `--log-group`, then the AWS default.
    pub(crate) fn log_group(&self, image: &str) -> String {
        self.log_group
            .clone()
            .unwrap_or_else(|| format!("{DEFAULT_GROUP_PREFIX}/{image}"))
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT)
    }

    /// The events one read reports: the newest in the stream, or every event
    /// since the configured lookback.
    pub(crate) fn window(&self, now: DateTime) -> LogWindow {
        self.since.map_or(LogWindow::Newest, |since| {
            LogWindow::Since(lookback(now, since))
        })
    }

    /// The same, for a read that is followed: everything from the oldest event
    /// onwards, which is the only direction that can be continued.
    pub(crate) fn follow_window(&self, now: DateTime) -> LogWindow {
        self.since.map_or(LogWindow::Everything, |since| {
            LogWindow::Since(lookback(now, since))
        })
    }
}

fn parse_microvm_id(value: &str) -> Result<String, String> {
    static MICROVM_ID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\Amicrovm-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z")
            .expect("valid MicroVM ID regex")
    });

    if MICROVM_ID.is_match(value) {
        Ok(value.to_owned())
    } else {
        Err("expected a complete MicroVM ID (microvm-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx), not a log stream name".into())
    }
}

async fn resolve<T: MicroVmClient>(
    client: &T,
    settings: &LogsSettings,
    group: String,
    microvm_id: &str,
) -> Result<Destination, ClankerError> {
    let streams = client.log_streams(&group).await?;
    let suffix = format!("]{microvm_id}");
    let mut matches: Vec<String> = streams
        .into_iter()
        .filter(|stream| stream.ends_with(&suffix))
        .collect();
    matches.sort();
    matches.dedup();
    match matches.len() {
        0 => Err(ClankerError::LogStreamNotFound {
            group,
            microvm_id: microvm_id.into(),
        }),
        1 => Ok(Destination::new(settings, group, matches.remove(0))),
        _ => Err(ClankerError::AmbiguousLogStreams {
            group,
            microvm_id: microvm_id.into(),
            streams: matches,
        }),
    }
}

/// The current instant, for resolving a lookback.
fn now() -> DateTime {
    DateTime::from(SystemTime::now())
}

/// `now` moved back by `since`, at the millisecond precision events are stored
/// with.
fn lookback(now: DateTime, since: Duration) -> DateTime {
    let since = i64::try_from(since.as_millis()).unwrap_or(i64::MAX);
    DateTime::from_millis(now.to_millis().unwrap_or_default().saturating_sub(since))
}

/// The stream one `logs` invocation reads, and how it reads it.
struct Destination {
    group: String,
    stream: String,
    limit: usize,
}

impl Destination {
    fn new(settings: &LogsSettings, group: String, stream: String) -> Self {
        Self {
            group,
            stream,
            limit: settings.limit(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogsResult {
    pub log_group: String,
    pub log_stream: String,
    pub events: Vec<LogEvent>,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &LogsOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    parse_microvm_id(&options.microvm_id).map_err(ClankerError::InvalidConfig)?;
    if options.follow && matches!(format, OutputFormat::Json) {
        return Err(ClankerError::InvalidConfig(
            "--follow prints events as they arrive and cannot be combined with --format json"
                .into(),
        ));
    }
    let settings = config.logs.merge(&options.settings)?;
    let now = now();
    let destination = resolve(
        client,
        &settings,
        settings.log_group(&config.name),
        &options.microvm_id,
    )
    .await?;

    if options.follow {
        eprintln!(
            "Following {} in {}; press Ctrl-C to stop",
            destination.stream, destination.group
        );
        let mut stdout = std::io::stdout();
        return follow(
            client,
            &destination,
            settings.follow_window(now),
            options.raw,
            tokio::signal::ctrl_c(),
            &mut stdout,
        )
        .await;
    }

    let result = LogsResult {
        log_group: destination.group.clone(),
        log_stream: destination.stream.clone(),
        events: snapshot(client, &destination, settings.window(now)).await?,
    };
    render(format, &result, || human(&result, !options.raw))
}

/// Reads the events the window covers, oldest first, at most `limit` of them.
async fn snapshot<T: MicroVmClient>(
    client: &T,
    destination: &Destination,
    window: LogWindow,
) -> Result<Vec<LogEvent>, ClankerError> {
    let mut events = Vec::new();
    let mut next_token = None;
    loop {
        let page = read(client, destination, window.clone(), next_token).await?;
        let empty = page.events.is_empty();
        events.extend(page.events);
        next_token = match page.next_token {
            // A backward read ends at the newest event, and an empty page means
            // the window is exhausted, so only a forward read continues.
            Some(token) if window.is_forward() && !empty && events.len() < destination.limit => {
                Some(token)
            }
            _ => break,
        };
    }
    events.truncate(destination.limit);
    events.sort_by_key(|event| event.timestamp);
    Ok(events)
}

/// Prints events as they arrive until `stop` resolves.
async fn follow<T, S, W>(
    client: &T,
    destination: &Destination,
    window: LogWindow,
    raw: bool,
    stop: S,
    out: &mut W,
) -> Result<(), ClankerError>
where
    T: MicroVmClient,
    S: Future,
    W: Write,
{
    let mut stop = pin!(stop);
    let mut next_token = None;
    loop {
        let page = read(client, destination, window.clone(), next_token).await?;
        write_events(out, &page.events, !raw)?;
        next_token = page.next_token;
        tokio::select! {
            _ = &mut stop => return Ok(()),
            () = tokio::time::sleep(FOLLOW_INTERVAL) => {}
        }
    }
}

/// One page of events, with the failures `logs` reports for them.
async fn read<T: MicroVmClient>(
    client: &T,
    destination: &Destination,
    window: LogWindow,
    next_token: Option<String>,
) -> Result<LogPage, ClankerError> {
    let query = LogQuery {
        group: destination.group.clone(),
        stream: destination.stream.clone(),
        window,
        limit: destination.limit,
        next_token,
    };
    Ok(client.log_events(&query).await?)
}

/// Writes events as terminal lines and flushes them.
fn write_events<W: Write>(
    out: &mut W,
    events: &[LogEvent],
    timestamps: bool,
) -> Result<(), ClankerError> {
    for event in events {
        write!(out, "{}", line(event, timestamps)).map_err(write_error)?;
    }
    out.flush().map_err(write_error)
}

fn write_error(source: std::io::Error) -> ClankerError {
    ClankerError::Io {
        action: "write logs".into(),
        source,
    }
}

/// One event as a line of output, ending in a newline even when the message has
/// none.
fn line(event: &LogEvent, timestamps: bool) -> String {
    let mut text = if timestamps {
        format!("{}  {}", event.timestamp, event.message)
    } else {
        event.message.clone()
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The whole snapshot as one block of text.
fn human(result: &LogsResult, timestamps: bool) -> String {
    if result.events.is_empty() {
        return format!(
            "No log events in `{}` of `{}`.",
            result.log_stream, result.log_group
        );
    }
    let text: String = result
        .events
        .iter()
        .map(|event| line(event, timestamps))
        .collect();
    text.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient};
    use crate::test_support::{LogEventBuilder, ROLE, project};
    use tempfile::TempDir;

    const ID: &str = "microvm-f4e3b5a1-3a16-3f63-8470-251708859820";
    const STREAM: &str = "2026/09/11[13.0]microvm-f4e3b5a1-3a16-3f63-8470-251708859820";

    /// A project whose `[microvm.run]` role resolves the account, like every command.
    fn config(sections: &str) -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let config = project(
            directory.path(),
            &format!("[microvm.run]\n{ROLE}{sections}"),
        );
        (directory, config)
    }

    fn options(microvm_id: &str) -> LogsOptions {
        LogsOptions {
            microvm_id: microvm_id.into(),
            ..LogsOptions::default()
        }
    }

    fn destination(options: &LogsOptions, config: &ProjectConfig) -> Destination {
        let settings = config.logs.merge(&options.settings).unwrap();
        Destination::new(&settings, settings.log_group(&config.name), STREAM.into())
    }

    fn page(events: impl IntoIterator<Item = LogEvent>, next_token: Option<&str>) -> LogPage {
        LogPage {
            events: events.into_iter().collect(),
            next_token: next_token.map(str::to_owned),
        }
    }

    fn read_queries(calls: &[Call]) -> Vec<LogQuery> {
        calls
            .iter()
            .map(|call| {
                let Call::LogEvents(query) = call else {
                    panic!("expected a read, got {call:?}");
                };
                query.clone()
            })
            .collect()
    }

    fn messages(events: &[LogEvent]) -> Vec<&str> {
        events.iter().map(|event| event.message.as_str()).collect()
    }

    /// Asserts a window starts `seconds` before now, whatever the clock did.
    fn assert_lookback(window: &LogWindow, seconds: u64) {
        let LogWindow::Since(start) = window else {
            panic!("expected a window since a start time, got {window:?}");
        };
        let looked_back = now().to_millis().unwrap() - start.to_millis().unwrap();
        let expected = i64::try_from(seconds).unwrap() * 1000;
        assert!(
            (looked_back - expected).abs() < 5_000,
            "looked back {looked_back}ms instead of {expected}ms"
        );
    }

    #[tokio::test]
    async fn the_newest_events_are_read_from_the_run_group() {
        let (_directory, config) = config("[microvm.run.logs]\ngroup = \"/demo/runs\"\n");
        let client = FakeMicroVmClient::default()
            .log_streams([Ok(vec!["unrelated-stream".into(), STREAM.into()])])
            .log_events([Ok(page(
                [
                    LogEventBuilder::new("first\n").at(10).build(),
                    LogEventBuilder::new("second\n").at(20).build(),
                ],
                None,
            ))]);
        let destination = resolve(
            &client,
            &config.logs,
            config.logs.log_group(&config.name),
            ID,
        )
        .await
        .unwrap();
        let events = snapshot(&client, &destination, LogWindow::Newest)
            .await
            .unwrap();

        assert_eq!(messages(&events), ["first\n", "second\n"]);
        let queries = read_queries(&client.calls()[1..]);
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].group, "/demo/runs");
        assert_eq!(queries[0].stream, STREAM);
        assert_eq!(queries[0].window, LogWindow::Newest);
        assert_eq!(queries[0].limit, DEFAULT_LIMIT);
        assert_eq!(queries[0].next_token, None);
    }

    #[tokio::test]
    async fn without_configuration_aws_defaults_are_used() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default();
        let options = options(ID);

        snapshot(&client, &destination(&options, &config), LogWindow::Newest)
            .await
            .unwrap();

        let queries = read_queries(&client.calls());
        assert_eq!(queries[0].group, "/aws/lambda-microvms/demo");
        assert_eq!(queries[0].stream, STREAM);
    }

    #[tokio::test]
    async fn flags_override_the_shared_run_logs_table() {
        let (_directory, config) =
            config("[microvm.run.logs]\ngroup = \"/demo/other\"\nlimit = 5\n");
        let client = FakeMicroVmClient::default();

        let configured = options(ID);
        snapshot(
            &client,
            &destination(&configured, &config),
            LogWindow::Newest,
        )
        .await
        .unwrap();

        let mut flagged = options(ID);
        flagged.settings.log_group = Some("/demo/cli".into());
        snapshot(&client, &destination(&flagged, &config), LogWindow::Newest)
            .await
            .unwrap();

        let queries = read_queries(&client.calls());
        assert_eq!(queries[0].group, "/demo/other");
        assert_eq!(queries[0].stream, STREAM);
        assert_eq!(queries[0].limit, 5);
        assert_eq!(queries[1].group, "/demo/cli");
        assert_eq!(queries[1].stream, STREAM);
    }

    #[tokio::test]
    async fn since_reads_forward_and_stops_at_the_limit() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default().log_events([
            Ok(page(
                [
                    LogEventBuilder::new("a").at(30).build(),
                    LogEventBuilder::new("b").at(10).build(),
                ],
                Some("token-1"),
            )),
            Ok(page(
                [
                    LogEventBuilder::new("c").at(50).build(),
                    LogEventBuilder::new("d").at(40).build(),
                ],
                Some("token-2"),
            )),
        ]);
        let mut options = options(ID);
        options.settings.since = Some(Duration::from_secs(600));
        options.settings.limit = Some(3);
        let settings = config.logs.merge(&options.settings).unwrap();

        let events = snapshot(
            &client,
            &destination(&options, &config),
            settings.window(now()),
        )
        .await
        .unwrap();

        // The page that crosses the limit is kept up to it, oldest first.
        assert_eq!(messages(&events), ["b", "a", "c"]);
        let queries = read_queries(&client.calls());
        assert_eq!(queries.len(), 2);
        assert!(queries[0].window.is_forward());
        assert_eq!(queries[0].next_token, None);
        assert_eq!(queries[1].next_token.as_deref(), Some("token-1"));
        assert_eq!(queries[1].limit, 3);
        assert_lookback(&queries[0].window, 600);
    }

    #[tokio::test]
    async fn a_backward_read_is_never_paginated() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default().log_events([Ok(page(
            [LogEventBuilder::new("only").build()],
            Some("token-1"),
        ))]);
        let options = options(ID);

        snapshot(&client, &destination(&options, &config), LogWindow::Newest)
            .await
            .unwrap();

        assert_eq!(read_queries(&client.calls()).len(), 1);
    }

    #[tokio::test]
    async fn a_forward_read_stops_when_a_page_brings_nothing_new() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default().log_events([
            Ok(page([LogEventBuilder::new("a").build()], Some("token-1"))),
            Ok(page([], Some("token-1"))),
        ]);
        let options = options(ID);

        let events = snapshot(
            &client,
            &destination(&options, &config),
            LogWindow::Everything,
        )
        .await
        .unwrap();

        assert_eq!(messages(&events), ["a"]);
        assert_eq!(read_queries(&client.calls()).len(), 2);
    }

    #[tokio::test]
    async fn follow_prints_each_event_once_and_keeps_reading() {
        let (_directory, config) = config("[microvm.run.logs]\ngroup = \"/demo/runs\"\n");
        let client = FakeMicroVmClient::default().log_events([
            Ok(page(
                [
                    LogEventBuilder::new("first\n").at(10).build(),
                    LogEventBuilder::new("second\n").at(20).build(),
                ],
                Some("token-1"),
            )),
            Ok(page(
                [LogEventBuilder::new("third\n").at(30).build()],
                Some("token-2"),
            )),
            Ok(page([], Some("token-2"))),
        ]);
        let destination = destination(&options(ID), &config);
        let mut out = Vec::new();
        let watched = client.clone();
        let stop = async move {
            while watched.calls().len() < 3 {
                tokio::task::yield_now().await;
            }
        };

        follow(
            &client,
            &destination,
            LogWindow::Everything,
            false,
            stop,
            &mut out,
        )
        .await
        .unwrap();

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("  first"), "{text}");
        assert!(lines[2].ends_with("  third"), "{text}");
        let queries = read_queries(&client.calls());
        assert_eq!(queries[0].next_token, None);
        assert_eq!(queries[1].next_token.as_deref(), Some("token-1"));
        assert_eq!(queries[2].next_token.as_deref(), Some("token-2"));
    }

    #[tokio::test]
    async fn follow_can_print_messages_without_timestamps() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default().log_events([Ok(page(
            [LogEventBuilder::new("hello").at(10).build()],
            None,
        ))]);
        let destination = destination(&options(ID), &config);
        let mut out = Vec::new();

        follow(
            &client,
            &destination,
            LogWindow::Everything,
            true,
            std::future::ready(()),
            &mut out,
        )
        .await
        .unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn follow_human_output_is_not_offered_as_json() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default();
        let mut options = options(ID);
        options.follow = true;

        let error = execute(&options, &config, OutputFormat::Json, &client)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot be combined with --format json"),
            "{error}"
        );
        assert!(client.calls().is_empty());
    }

    #[tokio::test]
    async fn an_empty_microvm_id_is_rejected_before_reading() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default();

        let error = execute(&options("  "), &config, OutputFormat::Human, &client)
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
        assert!(client.calls().is_empty());
    }

    #[test]
    fn unsupported_read_sizes_are_rejected() {
        let (_directory, config) = config("");
        let mut options = options(ID);
        options.settings.limit = Some(0);

        let error = config.logs.merge(&options.settings).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("microvm.run.logs.limit must be between 1 and 10000"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_empty_stream_is_reported_as_such() {
        let (_directory, config) = config("[microvm.run.logs]\ngroup = \"/demo/runs\"\n");
        let client = FakeMicroVmClient::default();
        let options = options(ID);
        let destination = destination(&options, &config);

        let result = LogsResult {
            log_group: destination.group.clone(),
            log_stream: destination.stream.clone(),
            events: snapshot(&client, &destination, LogWindow::Newest)
                .await
                .unwrap(),
        };

        assert_eq!(
            human(&result, true),
            format!("No log events in `{STREAM}` of `/demo/runs`.")
        );
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["logGroup"], "/demo/runs");
        assert_eq!(json["logStream"], STREAM);
        assert_eq!(json["events"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_minimal_project_needs_no_role_and_no_group() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), "");
        let client = FakeMicroVmClient::default();

        let result = LogsResult {
            log_group: config.logs.log_group(&config.name),
            log_stream: STREAM.into(),
            events: snapshot(
                &client,
                &destination(&options(ID), &config),
                LogWindow::Newest,
            )
            .await
            .unwrap(),
        };

        assert_eq!(result.log_group, "/aws/lambda-microvms/demo");
        assert_eq!(result.log_stream, STREAM);
        assert_eq!(
            human(&result, true),
            format!("No log events in `{STREAM}` of `/aws/lambda-microvms/demo`.")
        );
    }

    #[test]
    fn events_are_rendered_with_their_timestamps_unless_raw() {
        let result = LogsResult {
            log_group: "/demo/runs".into(),
            log_stream: STREAM.into(),
            events: vec![
                LogEventBuilder::new("hello").at(0).build(),
                LogEventBuilder::new("again\n").at(61).build(),
            ],
        };

        assert_eq!(
            human(&result, true),
            "1970-01-01T00:00:00Z  hello\n1970-01-01T00:01:01Z  again"
        );
        assert_eq!(human(&result, false), "hello\nagain");
    }

    #[test]
    fn the_configured_lookback_moves_the_window_back() {
        let (_directory, config) = config("[microvm.run.logs]\nsince = \"30m\"\n");

        assert_lookback(&config.logs.window(now()), 1800);
        assert_lookback(&config.logs.follow_window(now()), 1800);
    }

    #[test]
    fn a_window_without_a_lookback_reads_the_newest_events() {
        let (_directory, config) = config("");

        assert_eq!(config.logs.window(now()), LogWindow::Newest);
        assert_eq!(config.logs.follow_window(now()), LogWindow::Everything);
    }
}
