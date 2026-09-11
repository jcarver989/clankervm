use super::RunSettings;
use crate::client::{LogEvent, LogPage, LogQuery, LogWindow, MicroVmClient, MicroVmClientError};
use crate::config::{ProjectConfig, Settings};
use crate::output::render;
use crate::util::validate_non_empty;
use crate::{ClankerError, OutputFormat};
use aws_smithy_types::DateTime;
use clap::Args;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::io::Write;
use std::pin::pin;
use std::time::{Duration, SystemTime};

/// The log group AWS streams MicroVM logs to by default.
const DEFAULT_GROUP_PREFIX: &str = "/aws/lambda-microvms";
/// Events requested per read when `--limit` is not configured.
const DEFAULT_LIMIT: usize = 1000;
/// Events one read may ask for.
const MAX_LIMIT: usize = 10_000;
/// How long one read may take when `--timeout` is not configured.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `--follow` waits before asking for more events.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Args)]
pub struct LogsOptions {
    /// MicroVM to read logs for, as reported by run or list.
    #[arg(value_name = "MICROVM_ID")]
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

/// Logs settings, shared by `--flags` and the `[logs]` table.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct LogsSettings {
    /// Log group to read; defaults to the group run streams to.
    #[arg(long)]
    pub log_group: Option<String>,
    /// Log stream to read; defaults to the MicroVM id.
    #[arg(long)]
    pub log_stream: Option<String>,
    /// Read events from this long before now, for example 30m.
    #[arg(long, value_parser = humantime::parse_duration)]
    #[serde(with = "humantime_serde")]
    pub since: Option<Duration>,
    /// Events to report per read; defaults to 1000, at most 10000.
    #[arg(long)]
    pub limit: Option<usize>,
    /// How long one read may take; defaults to 1m.
    #[arg(long, value_parser = humantime::parse_duration)]
    #[serde(with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

impl Settings for LogsSettings {
    fn validate(&self) -> Result<(), ClankerError> {
        validate_non_empty(self.log_group.as_deref(), "logs.log-group")?;
        validate_non_empty(self.log_stream.as_deref(), "logs.log-stream")?;
        if self
            .limit
            .is_some_and(|limit| !(1..=MAX_LIMIT).contains(&limit))
        {
            return Err(ClankerError::InvalidConfig(format!(
                "logs.limit must be between 1 and {MAX_LIMIT}"
            )));
        }
        Ok(())
    }
}

impl LogsSettings {
    /// The group to read: the `[logs]` table or `--log-group`, then the group
    /// `run` streams to, then the one AWS uses by default.
    pub(crate) fn log_group(&self, run: &RunSettings, image: &str) -> String {
        self.log_group
            .clone()
            .or_else(|| run.log_group.clone())
            .unwrap_or_else(|| format!("{DEFAULT_GROUP_PREFIX}/{image}"))
    }

    /// The stream to read, which AWS names after the MicroVM by default.
    pub(crate) fn log_stream(&self, microvm_id: &str) -> String {
        self.log_stream
            .clone()
            .unwrap_or_else(|| microvm_id.to_owned())
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT)
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
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
    timeout: Duration,
}

impl Destination {
    fn new(settings: &LogsSettings, config: &ProjectConfig, microvm_id: &str) -> Self {
        Self {
            group: settings.log_group(&config.run, &config.image.name),
            stream: settings.log_stream(microvm_id),
            limit: settings.limit(),
            timeout: settings.timeout(),
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
    validate_non_empty(Some(&options.microvm_id), "MicroVM id")?;
    if options.follow && matches!(format, OutputFormat::Json) {
        return Err(ClankerError::InvalidConfig(
            "--follow prints events as they arrive and cannot be combined with --format json"
                .into(),
        ));
    }
    let settings = config.logs.merge(&options.settings)?;
    let destination = Destination::new(&settings, config, &options.microvm_id);
    let now = now();

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
    match tokio::time::timeout(destination.timeout, client.log_events(&query)).await {
        Err(_) => Err(ClankerError::LogsTimeout {
            timeout: destination.timeout,
            group: destination.group.clone(),
            stream: destination.stream.clone(),
        }),
        Ok(Ok(page)) => Ok(page),
        Ok(Err(MicroVmClientError::NoLogStream { .. })) => {
            Err(not_found(client, destination).await)
        }
        Ok(Err(error)) => Err(error.into()),
    }
}

/// The streams the group does have, so a wrong stream name is obvious.
async fn not_found<T: MicroVmClient>(client: &T, destination: &Destination) -> ClankerError {
    let streams = client
        .log_streams(&destination.group)
        .await
        .unwrap_or_default();
    ClankerError::LogStreamNotFound {
        group: destination.group.clone(),
        stream: destination.stream.clone(),
        streams,
    }
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

    /// A project whose `[run]` role resolves the account, like every command.
    fn config(sections: &str) -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), &format!("[run]\n{ROLE}{sections}"));
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
        Destination::new(&settings, config, &options.microvm_id)
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
        let (_directory, config) = config("log-group = \"/demo/runs\"\n");
        let client = FakeMicroVmClient::default().log_events([Ok(page(
            [
                LogEventBuilder::new("first\n").at(10).build(),
                LogEventBuilder::new("second\n").at(20).build(),
            ],
            None,
        ))]);
        let options = options("microvm-7");

        let events = snapshot(&client, &destination(&options, &config), LogWindow::Newest)
            .await
            .unwrap();

        assert_eq!(messages(&events), ["first\n", "second\n"]);
        let queries = read_queries(&client.calls());
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].group, "/demo/runs");
        assert_eq!(queries[0].stream, "microvm-7");
        assert_eq!(queries[0].window, LogWindow::Newest);
        assert_eq!(queries[0].limit, DEFAULT_LIMIT);
        assert_eq!(queries[0].next_token, None);
    }

    #[tokio::test]
    async fn without_configuration_aws_defaults_are_used() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default();
        let options = options("microvm-7");

        snapshot(&client, &destination(&options, &config), LogWindow::Newest)
            .await
            .unwrap();

        let queries = read_queries(&client.calls());
        assert_eq!(queries[0].group, "/aws/lambda-microvms/demo");
        assert_eq!(queries[0].stream, "microvm-7");
    }

    #[tokio::test]
    async fn the_logs_table_and_the_flags_override_the_run_group() {
        let (_directory, config) = config(
            "log-group = \"/demo/runs\"\n[logs]\nlog-group = \"/demo/other\"\nlog-stream = \"stack\"\nlimit = 5\n",
        );
        let client = FakeMicroVmClient::default();

        let configured = options("microvm-7");
        snapshot(
            &client,
            &destination(&configured, &config),
            LogWindow::Newest,
        )
        .await
        .unwrap();

        let mut flagged = options("microvm-7");
        flagged.settings.log_group = Some("/demo/cli".into());
        flagged.settings.log_stream = Some("custom".into());
        snapshot(&client, &destination(&flagged, &config), LogWindow::Newest)
            .await
            .unwrap();

        let queries = read_queries(&client.calls());
        assert_eq!(queries[0].group, "/demo/other");
        assert_eq!(queries[0].stream, "stack");
        assert_eq!(queries[0].limit, 5);
        assert_eq!(queries[1].group, "/demo/cli");
        assert_eq!(queries[1].stream, "custom");
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
        let mut options = options("microvm-7");
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
        let options = options("microvm-7");

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
        let options = options("microvm-7");

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
    async fn a_missing_stream_names_the_streams_the_group_has() {
        let (_directory, config) = config("log-group = \"/demo/runs\"\n");
        let client = FakeMicroVmClient::default()
            .log_events([Err(MicroVmClientError::NoLogStream {
                group: "/demo/runs".into(),
                stream: "microvm-7".into(),
            })])
            .log_streams([Ok(vec!["job-1".into(), "job-2".into()])]);
        let options = options("microvm-7");

        let error = snapshot(&client, &destination(&options, &config), LogWindow::Newest)
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("no log stream `microvm-7` in log group `/demo/runs`"),
            "{message}"
        );
        assert!(message.contains("the group has: job-1, job-2"), "{message}");
        assert!(message.contains("--log-stream"), "{message}");
    }

    #[test]
    fn only_the_first_streams_a_group_has_are_named() {
        let streams = (1..=9).map(|index| format!("job-{index}")).collect();

        let message = ClankerError::LogStreamNotFound {
            group: "/demo/runs".into(),
            stream: "microvm-7".into(),
            streams,
        }
        .to_string();

        assert!(
            message.contains("the group has: job-1, job-2, job-3, job-4, job-5"),
            "{message}"
        );
        assert!(!message.contains("job-6"), "{message}");
    }

    #[test]
    fn a_missing_group_is_reported_without_streams() {
        let message = ClankerError::LogStreamNotFound {
            group: "/demo/runs".into(),
            stream: "microvm-7".into(),
            streams: Vec::new(),
        }
        .to_string();

        assert!(
            message.starts_with("no log stream `microvm-7` in log group `/demo/runs`;"),
            "{message}"
        );
        assert!(!message.contains("the group has"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_takes_too_long_is_reported() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default().with_delay(Duration::from_secs(10));
        let mut options = options("microvm-7");
        options.settings.timeout = Some(Duration::from_secs(1));

        let error = snapshot(&client, &destination(&options, &config), LogWindow::Newest)
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::LogsTimeout { .. }), "{error}");
    }

    #[tokio::test]
    async fn follow_prints_each_event_once_and_keeps_reading() {
        let (_directory, config) = config("log-group = \"/demo/runs\"\n");
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
        let destination = destination(&options("microvm-7"), &config);
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
        let destination = destination(&options("microvm-7"), &config);
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
        let mut options = options("microvm-7");
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
        let mut options = options("microvm-7");
        options.settings.limit = Some(0);

        let error = config.logs.merge(&options.settings).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("logs.limit must be between 1 and 10000"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_empty_stream_is_reported_as_such() {
        let (_directory, config) = config("log-group = \"/demo/runs\"\n");
        let client = FakeMicroVmClient::default();
        let options = options("microvm-7");
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
            "No log events in `microvm-7` of `/demo/runs`."
        );
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["logGroup"], "/demo/runs");
        assert_eq!(json["logStream"], "microvm-7");
        assert_eq!(json["events"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_minimal_project_needs_no_role_and_no_group() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), "");
        let client = FakeMicroVmClient::default();

        let result = LogsResult {
            log_group: config.logs.log_group(&config.run, &config.image.name),
            log_stream: config.logs.log_stream("microvm-7"),
            events: snapshot(
                &client,
                &destination(&options("microvm-7"), &config),
                LogWindow::Newest,
            )
            .await
            .unwrap(),
        };

        assert_eq!(result.log_group, "/aws/lambda-microvms/demo");
        assert_eq!(result.log_stream, "microvm-7");
        assert_eq!(
            human(&result, true),
            "No log events in `microvm-7` of `/aws/lambda-microvms/demo`."
        );
    }

    #[test]
    fn events_are_rendered_with_their_timestamps_unless_raw() {
        let result = LogsResult {
            log_group: "/demo/runs".into(),
            log_stream: "microvm-7".into(),
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
        let (_directory, config) = config("[logs]\nsince = \"30m\"\n");

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
