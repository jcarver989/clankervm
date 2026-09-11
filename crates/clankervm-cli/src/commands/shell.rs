use super::run::{RunOptions, plan_launch};
use crate::client::{MicroVmClient, MicroVmDetails, SHELL_INGRESS};
use crate::config::ProjectConfig;
use crate::shell::{Event, Options, ShellError, attach, connect, events, is_interactive, request};
use crate::{ClankerError, OutputFormat};
use aws_sdk_lambdamicrovms::types::MicrovmState;
use clap::Args;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;

/// How often AWS is asked about a MicroVM that is still starting or stopping.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long AWS is given to confirm a termination it accepted.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(30);
/// What a launched MicroVM runs when nothing else is asked of it: enough to
/// keep it alive for the shell.
const KEEP_ALIVE: [&str; 2] = ["sleep", "infinity"];

#[derive(Debug, Args)]
pub struct ShellOptions {
    /// MicroVM to attach to; omit it to launch a new one.
    ///
    /// Nothing is launched with an id, so the launch flags conflict with it:
    /// `RunSettings` is the group clap derives for the flattened settings, and
    /// the rest are the arguments `RunOptions` adds on top of them.
    #[arg(
        value_name = "MICROVM_ID",
        conflicts_with_all = ["RunSettings", "arguments", "release", "client_token"]
    )]
    pub microvm_id: Option<String>,
    /// How long to wait for a launched MicroVM to start.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "5m")]
    pub timeout: Duration,
    /// Leave a launched MicroVM running when the session ends.
    #[arg(long, conflicts_with = "microvm_id")]
    pub keep: bool,
    #[command(flatten)]
    pub run: RunOptions,
}

/// What one `shell` invocation did.
#[derive(Debug)]
pub(crate) struct ShellResult {
    pub microvm_id: String,
    pub launched: bool,
    pub terminated: bool,
    pub detached: bool,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &ShellOptions,
    config: &ProjectConfig,
    client: &T,
) -> Result<(), ClankerError> {
    let mut events = events();
    let mut out = tokio::io::stdout();
    let result = shell(
        options,
        config,
        client,
        connect,
        Options::local(),
        &mut events,
        &mut out,
    )
    .await?;
    eprintln!("{}", human(&result));
    Ok(())
}

/// Rejects a `shell` invocation without a terminal to attach, before the
/// project file or AWS credentials are touched.
pub(super) fn preflight(format: OutputFormat) -> Result<(), ClankerError> {
    if matches!(format, OutputFormat::Json) {
        return Err(ClankerError::InvalidConfig(
            "`shell` cannot be combined with --format json; it requires an interactive terminal"
                .into(),
        ));
    }
    if is_interactive() {
        return Ok(());
    }
    Err(ClankerError::NotATerminal)
}

/// Runs one shell session: attach to an existing MicroVM, or launch one and
/// stop it again when the session ends.
pub(crate) async fn shell<T, U, V>(
    options: &ShellOptions,
    config: &ProjectConfig,
    client: &T,
    connect: impl AsyncFnOnce(ClientRequestBuilder) -> Result<WebSocketStream<U>, ShellError>,
    session: Options,
    events: &mut mpsc::Receiver<Event>,
    out: &mut V,
) -> Result<ShellResult, ClankerError>
where
    T: MicroVmClient,
    U: AsyncRead + AsyncWrite + Unpin,
    V: AsyncWrite + Unpin,
{
    if options.run.settings.ingress.is_some() {
        return Err(ClankerError::InvalidConfig(format!(
            "`shell` always uses the {SHELL_INGRESS} ingress connector; drop --ingress"
        )));
    }
    let launched = options.microvm_id.is_none();
    let microvm_id = match &options.microvm_id {
        Some(microvm_id) => microvm_id.clone(),
        None => launch(options, config, client).await?,
    };
    let outcome = async {
        let details = ready(client, &microvm_id, launched, options.timeout, events).await?;
        let token = client.shell_token(&microvm_id).await?;
        // The terminal is restored as the session ends, before the MicroVM is
        // stopped and before anything else is printed.
        attach(
            connect(request(&details.endpoint, &token)?),
            events,
            out,
            session,
        )
        .await
        .map_err(ClankerError::from)
    }
    .await;
    // A launched MicroVM is stopped however the session ended, so a failure
    // inside the session does not leave one behind.
    let terminated = launched && !options.keep;
    let cleanup = if terminated {
        terminate(client, &microvm_id).await
    } else {
        Ok(())
    };
    let outcome = outcome?;
    cleanup?;
    Ok(ShellResult {
        microvm_id,
        launched,
        terminated,
        detached: outcome.detached,
    })
}

/// Launches the MicroVM the session attaches to, exactly as `run` would, with
/// the connector that exposes its pty and a command that keeps it alive.
async fn launch<T: MicroVmClient>(
    options: &ShellOptions,
    config: &ProjectConfig,
    client: &T,
) -> Result<String, ClankerError> {
    let mut run = options.run.clone();
    run.settings.ingress = Some(SHELL_INGRESS.to_owned());
    if run.arguments.is_empty() && config.run.command.is_none() {
        run.arguments = KEEP_ALIVE.iter().map(|word| (*word).to_owned()).collect();
    }
    let plan = plan_launch(&run, config)?;
    let launched = client.launch(&plan.spec).await?;
    eprintln!(
        "Launched MicroVM {}; exit the shell or press Ctrl-] to terminate it",
        launched.microvm_id
    );
    Ok(launched.microvm_id)
}

/// Waits for the MicroVM to be ready to attach to, checking on the way that it
/// was launched with the connector that exposes its pty. Ctrl-C during the
/// wait gives up on it.
async fn ready<T: MicroVmClient>(
    client: &T,
    microvm_id: &str,
    launched: bool,
    timeout: Duration,
    events: &mut mpsc::Receiver<Event>,
) -> Result<MicroVmDetails, ClankerError> {
    let polled = poll(client, microvm_id, timeout, |described| match described {
        Some(details) if details.state == MicrovmState::Running => {
            ensure_shell_ingress(&details)?;
            Ok(Some(details))
        }
        // AWS keeps a MicroVM PENDING for a while before it can be attached
        // to, and may not have registered it at all yet.
        None if launched => Ok(None),
        Some(details) if launched && details.state == MicrovmState::Pending => Ok(None),
        Some(details) => Err(ClankerError::MicroVmNotRunning {
            microvm_id: microvm_id.to_owned(),
            state: details.state.to_string(),
            reason: details.state_reason,
        }),
        None => Err(ClankerError::MicroVmNotFound(microvm_id.to_owned())),
    });
    let details = tokio::select! {
        polled = polled => polled?,
        () = interrupting(events) => return Err(ShellError::Interrupted.into()),
    };
    details.ok_or_else(|| ClankerError::MicroVmWaitTimeout {
        microvm_id: microvm_id.to_owned(),
        timeout,
    })
}

/// Stops the MicroVM and waits for AWS to confirm it.
async fn terminate<T: MicroVmClient>(client: &T, microvm_id: &str) -> Result<(), ClankerError> {
    client.terminate(microvm_id).await?;
    let confirmed = poll(client, microvm_id, TERMINATE_TIMEOUT, |described| {
        Ok(match described {
            None => Some(()),
            Some(details) if details.state == MicrovmState::Terminated => Some(()),
            Some(_) => None,
        })
    })
    .await?;
    confirmed.ok_or_else(|| ClankerError::MicroVmTerminationUnconfirmed(microvm_id.to_owned()))
}

/// Describes the MicroVM every poll interval until `check` finds what it is
/// waiting for, or `None` once `timeout` has passed.
async fn poll<T, U>(
    client: &T,
    microvm_id: &str,
    duration: Duration,
    mut check: impl FnMut(Option<MicroVmDetails>) -> Result<Option<U>, ClankerError>,
) -> Result<Option<U>, ClankerError>
where
    T: MicroVmClient,
{
    timeout(duration, async {
        loop {
            if let Some(found) = check(client.describe(microvm_id).await?)? {
                return Ok(Some(found));
            }
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or(Ok(None))
}

/// Resolves when the user asks the process to stop, and never once the terminal
/// is gone.
async fn interrupting(events: &mut mpsc::Receiver<Event>) {
    loop {
        match events.recv().await {
            Some(Event::Interrupt) => return,
            Some(_) => {}
            None => std::future::pending::<()>().await,
        }
    }
}

/// Rejects a MicroVM whose pty AWS does not expose.
fn ensure_shell_ingress(details: &MicroVmDetails) -> Result<(), ClankerError> {
    if details
        .ingress_network_connectors
        .iter()
        .any(|connector| connector.ends_with(SHELL_INGRESS))
    {
        return Ok(());
    }
    Err(ClankerError::InvalidConfig(format!(
        "MicroVM {} was not launched with the {SHELL_INGRESS} ingress connector, \
         so it has no shell to attach to; launch one with `clankervm shell`",
        details.microvm_id
    )))
}

/// The line printed once the session is over, on standard error.
fn human(result: &ShellResult) -> String {
    let mut text = if result.detached {
        format!("✓ Detached from MicroVM {}", result.microvm_id)
    } else {
        format!("✓ The shell of MicroVM {} exited", result.microvm_id)
    };
    if result.terminated {
        text.push_str("; the MicroVM is terminated");
    } else if result.launched {
        text.push_str("; the MicroVM is still running");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{
        Call, FakeMicroVmClient, Launch, LaunchSpec, MicroVmClientError, ShellToken,
    };
    use crate::shell::{Remote, scripted};
    use crate::test_support::{MicroVmDetailsBuilder, ROLE, project};
    use futures_util::SinkExt;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::io::DuplexStream;
    use tokio::time::Instant;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// The microvm every scripted test launches.
    const LAUNCHED: &str = "microvm-9";
    /// The microvm every scripted test attaches to.
    const ATTACHED: &str = "microvm-1";

    /// A project whose `[microvm.run]` role resolves the account, like every command.
    fn config() -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), &format!("[microvm.run]\n{ROLE}"));
        (directory, config)
    }

    /// Shell options for one mode; `None` launches a MicroVM.
    fn options(microvm_id: Option<&str>) -> ShellOptions {
        ShellOptions {
            microvm_id: microvm_id.map(str::to_owned),
            timeout: Duration::from_mins(5),
            keep: false,
            run: RunOptions::default(),
        }
    }

    /// A remote end that sends AWS's preamble and then exits.
    /// The remote must outlive the session.
    async fn session() -> (WebSocketStream<DuplexStream>, Remote) {
        let (socket, mut remote) = scripted().await;
        remote.init().await;
        remote.close().await;
        (socket, remote)
    }

    fn launch() -> Launch {
        Launch {
            microvm_id: LAUNCHED.into(),
            image_version: "2".into(),
        }
    }

    /// A client that reports one running MicroVM however often it is asked.
    fn running(microvm_id: &str) -> FakeMicroVmClient {
        let details = Ok(Some(MicroVmDetailsBuilder::new(microvm_id).build()));
        FakeMicroVmClient::default().described(std::iter::repeat_n(details, 10))
    }

    /// A client that launches one MicroVM, which then runs.
    fn launching() -> FakeMicroVmClient {
        running(LAUNCHED).launched([Ok(launch())])
    }

    /// Runs one `shell` invocation against scripted AWS and a scripted remote.
    async fn run(
        options: &ShellOptions,
        config: &ProjectConfig,
        client: &FakeMicroVmClient,
        socket: WebSocketStream<DuplexStream>,
    ) -> Result<ShellResult, ClankerError> {
        let (_sender, mut events) = mpsc::channel(8);
        shell(
            options,
            config,
            client,
            async |_| Ok(socket),
            Options::default(),
            &mut events,
            &mut Vec::new(),
        )
        .await
    }

    /// The specification one launch sent, in the order the calls were made.
    fn launched(calls: &[Call]) -> LaunchSpec {
        for call in calls {
            if let Call::Launch(spec) = call {
                return spec.clone();
            }
        }
        panic!("expected a launch, got {calls:?}");
    }

    fn terminated(calls: &[Call]) -> usize {
        calls
            .iter()
            .filter(|call| matches!(call, Call::Terminate(_)))
            .count()
    }

    #[tokio::test(start_paused = true)]
    async fn attaching_describes_authenticates_and_leaves_the_microvm_alone() {
        let (_directory, config) = config();
        let client = running(ATTACHED).shell_tokens([Ok(ShellToken {
            headers: HashMap::from([("X-aws-proxy-auth".to_owned(), "scripted".to_owned())]),
        })]);
        let (socket, _remote) = session().await;

        let (_sender, mut events) = mpsc::channel(8);
        let result = shell(
            &options(Some(ATTACHED)),
            &config,
            &client,
            async |request: ClientRequestBuilder| {
                let request = request.into_client_request().unwrap();
                assert_eq!(request.uri(), "wss://example.test/shell");
                assert_eq!(request.headers()["X-aws-proxy-auth"], "scripted");
                Ok(socket)
            },
            Options::default(),
            &mut events,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.microvm_id, ATTACHED);
        assert!(!result.launched && !result.terminated && !result.detached);
        let calls = client.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(matches!(calls[0], Call::Describe(ref id) if id == ATTACHED));
        assert!(matches!(calls[1], Call::ShellToken(ref id) if id == ATTACHED));
    }

    #[tokio::test(start_paused = true)]
    async fn a_microvm_that_is_not_running_is_rejected() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default().described([Ok(Some(
            MicroVmDetailsBuilder::new(ATTACHED)
                .state(MicrovmState::Pending)
                .state_reason("starting up")
                .build(),
        ))]);
        let (socket, _remote) = session().await;

        let error = run(&options(Some(ATTACHED)), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(
            matches!(error, ClankerError::MicroVmNotRunning { .. }),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("is PENDING, not RUNNING (starting up)"),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_endpoint_that_is_not_a_host_is_reported() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default().described([Ok(Some(
            MicroVmDetailsBuilder::new(ATTACHED)
                .endpoint("http://example.test")
                .build(),
        ))]);
        let (socket, _remote) = session().await;

        let error = run(&options(Some(ATTACHED)), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(
            matches!(error, ClankerError::Shell(ShellError::InvalidEndpoint(_))),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_microvm_that_is_gone_is_reported() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default();
        let (socket, _remote) = session().await;

        let error = run(&options(Some(ATTACHED)), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::MicroVmNotFound(_)), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_microvm_without_the_shell_connector_is_rejected() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default().described([Ok(Some(
            MicroVmDetailsBuilder::new(ATTACHED)
                .ingress_network_connectors(["INTERNET_EGRESS"])
                .build(),
        ))]);
        let (socket, _remote) = session().await;

        let error = run(&options(Some(ATTACHED)), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
        assert!(error.to_string().contains(SHELL_INGRESS), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn launching_uses_the_shell_connector_and_stops_the_microvm() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default()
            .launched([Ok(launch())])
            .described([
                Ok(Some(
                    MicroVmDetailsBuilder::new(LAUNCHED)
                        .state(MicrovmState::Pending)
                        .build(),
                )),
                Ok(Some(MicroVmDetailsBuilder::new(LAUNCHED).build())),
                Ok(Some(
                    MicroVmDetailsBuilder::new(LAUNCHED)
                        .state(MicrovmState::Terminating)
                        .build(),
                )),
                Ok(Some(
                    MicroVmDetailsBuilder::new(LAUNCHED)
                        .state(MicrovmState::Terminated)
                        .build(),
                )),
            ]);
        let (socket, _remote) = session().await;

        let result = run(&options(None), &config, &client, socket).await.unwrap();

        assert_eq!(result.microvm_id, LAUNCHED);
        assert!(result.launched && result.terminated && !result.detached);
        let calls = client.calls();
        let spec = launched(&calls);
        assert!(
            spec.ingress_network_connector
                .as_str()
                .ends_with(SHELL_INGRESS),
            "{}",
            spec.ingress_network_connector
        );
        let payload: serde_json::Value = serde_json::from_str(&spec.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "sleep");
        assert_eq!(payload["args"], serde_json::json!(["infinity"]));
        assert_eq!(terminated(&calls), 1);
        assert!(
            matches!(calls.last(), Some(Call::Describe(id)) if id == LAUNCHED),
            "the termination is confirmed: {calls:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_after_the_separator_wins_over_the_default() {
        let (_directory, config) = config();
        let client = launching();
        let mut options = options(None);
        options.run.arguments = vec!["htop".into()];
        let (socket, _remote) = session().await;

        run(&options, &config, &client, socket).await.unwrap();

        let spec = launched(&client.calls());
        let payload: serde_json::Value = serde_json::from_str(&spec.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "htop");
        assert_eq!(payload["args"], serde_json::json!([]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_configured_command_wins_over_the_default() {
        let directory = TempDir::new().unwrap();
        let config = project(
            directory.path(),
            &format!("[microvm.run]\n{ROLE}\ncommand = [\"bash\", \"-l\"]"),
        );
        let client = launching();
        let (socket, _remote) = session().await;

        run(&options(None), &config, &client, socket).await.unwrap();

        let spec = launched(&client.calls());
        let payload: serde_json::Value = serde_json::from_str(&spec.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "bash");
        assert_eq!(payload["args"], serde_json::json!(["-l"]));
    }

    #[tokio::test(start_paused = true)]
    async fn keeping_the_microvm_leaves_it_running() {
        let (_directory, config) = config();
        let client = launching();
        let mut options = options(None);
        options.keep = true;
        let (socket, _remote) = session().await;

        let result = run(&options, &config, &client, socket).await.unwrap();

        assert!(result.launched && !result.terminated);
        assert_eq!(terminated(&client.calls()), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_that_fails_still_stops_the_microvm() {
        let (_directory, config) = config();
        let client = launching();
        let (socket, mut remote) = scripted().await;
        remote.socket.send(Message::text("{}")).await.unwrap();

        let error = run(&options(None), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(
            matches!(error, ClankerError::Shell(ShellError::Handshake)),
            "{error}"
        );
        assert_eq!(terminated(&client.calls()), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_termination_aws_never_confirms_is_an_error() {
        let (_directory, config) = config();
        let running = Ok(Some(MicroVmDetailsBuilder::new(LAUNCHED).build()));
        let stopping = Ok(Some(
            MicroVmDetailsBuilder::new(LAUNCHED)
                .state(MicrovmState::Terminating)
                .build(),
        ));
        let client = FakeMicroVmClient::default()
            .launched([Ok(launch())])
            .described(std::iter::once(running).chain(std::iter::repeat_n(stopping, 30)));
        let (socket, _remote) = session().await;

        let error = run(&options(None), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(
            matches!(error, ClankerError::MicroVmTerminationUnconfirmed(ref id) if id == LAUNCHED),
            "{error}"
        );
        assert_eq!(terminated(&client.calls()), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_microvm_that_never_starts_times_out_and_is_stopped() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default().launched([Ok(launch())]);
        let mut options = options(None);
        options.timeout = Duration::from_secs(5);
        let (socket, _remote) = session().await;

        let error = run(&options, &config, &client, socket).await.unwrap_err();

        assert!(
            matches!(error, ClankerError::MicroVmWaitTimeout { .. }),
            "{error}"
        );
        assert_eq!(terminated(&client.calls()), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_termination_that_fails_is_propagated() {
        let (_directory, config) = config();
        let client = launching().terminated([Err(MicroVmClientError::Service {
            operation: "terminate MicroVM",
            message: "AccessDeniedException".into(),
        })]);
        let (socket, _remote) = session().await;

        let error = run(&options(None), &config, &client, socket)
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("terminate MicroVM failed"),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn readiness_deadline_includes_a_slow_describe_call() {
        let duration = Duration::from_secs(5);
        let client = running(LAUNCHED).with_delay(Duration::from_secs(60));
        let (_sender, mut events) = mpsc::channel(8);
        let start = Instant::now();
        let error = ready(&client, LAUNCHED, true, duration, &mut events)
            .await
            .unwrap_err();
        assert!(matches!(error, ClankerError::MicroVmWaitTimeout { .. }));
        assert_eq!(start.elapsed(), duration);
    }

    #[tokio::test(start_paused = true)]
    async fn polling_deadline_includes_sleep_and_subsequent_calls() {
        let duration = Duration::from_secs(5);
        let client = FakeMicroVmClient::default().with_delay(Duration::from_secs(2));
        let start = Instant::now();
        let result = poll(&client, LAUNCHED, duration, |_| Ok(None::<()>))
            .await
            .unwrap();
        assert!(result.is_none());
        assert_eq!(start.elapsed(), duration);
        assert_eq!(client.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn session_error_takes_precedence_over_cleanup_error() {
        let (_directory, config) = config();
        let client = launching().terminated([Err(MicroVmClientError::Service {
            operation: "terminate MicroVM",
            message: "AccessDeniedException".into(),
        })]);
        let (socket, mut remote) = scripted().await;
        remote.socket.send(Message::text("{}")).await.unwrap();
        let error = run(&options(None), &config, &client, socket)
            .await
            .unwrap_err();
        assert!(matches!(error, ClankerError::Shell(ShellError::Handshake)));
        assert_eq!(terminated(&client.calls()), 1);
    }

    #[tokio::test]
    async fn the_shell_connector_cannot_be_overridden() {
        let (_directory, config) = config();
        let client = FakeMicroVmClient::default();
        let mut options = options(None);
        options.run.settings.ingress = Some("PUBLIC_INGRESS".into());
        let (socket, _remote) = session().await;

        let error = run(&options, &config, &client, socket).await.unwrap_err();

        assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
        assert!(error.to_string().contains("--ingress"), "{error}");
        assert!(client.calls().is_empty());
    }
}
