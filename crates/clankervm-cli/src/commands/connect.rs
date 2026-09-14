use crate::application::{client_arguments, preflight_client, prepare};
use crate::client::MicroVmClient;
use crate::config::{ConnectionSettings, ProjectConfig};
use crate::output::render;
use crate::readiness::wait_for_application;
use crate::{ClankerError, OutputFormat};
use clap::Args;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::Serialize;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::timeout;

const CHILD_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Args)]
pub struct ConnectOptions {
    /// Existing MicroVM to attach to.
    pub vm_id: String,
    /// Named [microvm.run.connect] entry.
    pub name: String,
    /// Override the configured readiness timeout.
    #[arg(long, value_parser = humantime::parse_duration)]
    pub connect_timeout: Option<Duration>,
    /// Extra arguments appended to the configured local client.
    #[arg(last = true, allow_hyphen_values = true)]
    pub arguments: Vec<String>,
}

pub(crate) struct SelectedConnection<'a> {
    pub settings: &'a ConnectionSettings,
    pub executable: Option<PathBuf>,
    pub timeout: Duration,
}

pub(crate) struct ConnectionSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl ConnectionSignals {
    pub(crate) fn new() -> Result<Self, ClankerError> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).map_err(signal_error)?,
            terminate: signal(SignalKind::terminate()).map_err(signal_error)?,
        })
    }

    pub(crate) async fn cancelled(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

fn signal_error(source: std::io::Error) -> ClankerError {
    ClankerError::Io {
        action: "listen for connection signals".into(),
        source,
    }
}

pub(super) fn preflight<'a>(
    name: &str,
    timeout: Option<Duration>,
    arguments: &[String],
    config: &'a ProjectConfig,
    format: OutputFormat,
) -> Result<SelectedConnection<'a>, ClankerError> {
    let settings = config.connection(name)?;
    let timeout = timeout.unwrap_or(settings.timeout);
    if !(Duration::from_secs(1)..=Duration::from_mins(15)).contains(&timeout) {
        return Err(ClankerError::InvalidConfig(
            "--connect-timeout must be between 1s and 15m".into(),
        ));
    }
    if settings.client.is_none() && !arguments.is_empty() {
        return Err(ClankerError::InvalidConfig(
            "trailing client arguments require a configured connection client".into(),
        ));
    }
    let executable = preflight_client(config, settings, format)?;
    Ok(SelectedConnection {
        settings,
        executable,
        timeout,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionResult<'a> {
    microvm_id: &'a str,
    connection: &'a str,
    port: u16,
    url: String,
    ready: bool,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &ConnectOptions,
    selected: SelectedConnection<'_>,
    config: &ProjectConfig,
    config_path: &Path,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let mut handed_off = false;
    let mut signals = ConnectionSignals::new()?;
    connect(
        client,
        ConnectionRequest {
            microvm_id: &options.vm_id,
            name: &options.name,
            trailing: &options.arguments,
            config,
            config_path,
            format,
            launched: false,
        },
        selected,
        &mut handed_off,
        &mut signals,
    )
    .await
}

pub(crate) struct ConnectionRequest<'a> {
    pub microvm_id: &'a str,
    pub name: &'a str,
    pub trailing: &'a [String],
    pub config: &'a ProjectConfig,
    pub config_path: &'a Path,
    pub format: OutputFormat,
    pub launched: bool,
}

pub(crate) async fn connect<T: MicroVmClient>(
    client: &T,
    request: ConnectionRequest<'_>,
    selected: SelectedConnection<'_>,
    handed_off: &mut bool,
    signals: &mut ConnectionSignals,
) -> Result<(), ClankerError> {
    let ConnectionRequest {
        microvm_id,
        name,
        trailing,
        config,
        config_path,
        format,
        launched,
    } = request;
    let ready = tokio::select! {
        result = wait_for_application(
            client,
            microvm_id,
            selected.settings,
            &config.aws.region,
            launched,
            selected.timeout,
        ) => result?,
        () = signals.cancelled() => return Err(ClankerError::Interrupted),
    };
    let Some(executable) = selected.executable else {
        let result = ConnectionResult {
            microvm_id,
            connection: name,
            port: selected.settings.port,
            url: ready.url.to_string(),
            ready: true,
        };
        let rendered = render(format, &result, || {
            format!(
                "✓ Connection `{name}` on MicroVM {microvm_id} is ready at {}",
                result.url
            )
        });
        if rendered.is_ok() {
            *handed_off = true;
            print_recovery_hints(microvm_id, name, config_path, &config.aws.region);
        }
        return rendered;
    };
    // Readiness credentials are intentionally not reused for the external client.
    let token = tokio::select! {
        result = client.application_token(microvm_id, selected.settings.port) => result?,
        () = signals.cancelled() => return Err(ClankerError::Interrupted),
    };
    let prepared = prepare(
        ready.url.origin().ascii_serialization().as_str(),
        &config.aws.region,
        &selected.settings.path,
        selected.settings.protocol,
        selected.settings.port,
        token.expose(),
    )?;
    let argv = client_arguments(selected.settings, &executable, &prepared, trailing);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|source| ClankerError::Io {
            action: "start connection client".into(),
            source,
        })?;
    *handed_off = true;
    print_recovery_hints(microvm_id, name, config_path, &config.aws.region);
    let status = wait_for_child(&mut child, signals).await?;
    if status.success() {
        Ok(())
    } else {
        Err(ClankerError::ClientExit(exit_code(status)))
    }
}

fn print_recovery_hints(microvm_id: &str, name: &str, config_path: &Path, region: &str) {
    let command = format!(
        "clankervm --config {} --region {region}",
        config_path.display()
    );
    eprintln!(
        "Connected to MicroVM {microvm_id} using `{name}`. The MicroVM will remain running after the client exits.\nLogs:      {command} logs {microvm_id}\nReconnect: {command} connect {microvm_id} {name}\nInspect:   {command} inspect {microvm_id}\nStop:      {command} stop {microvm_id} --wait"
    );
}

async fn wait_for_child(
    child: &mut Child,
    signals: &mut ConnectionSignals,
) -> Result<ExitStatus, ClankerError> {
    tokio::select! {
        result = child.wait() => result.map_err(wait_error),
        _ = signals.terminate.recv() => {
            terminate_child(child)?;
            if let Ok(result) = timeout(CHILD_SHUTDOWN_GRACE, child.wait()).await {
                result.map_err(wait_error)
            } else {
                child.start_kill().map_err(|source| ClankerError::Io {
                    action: "kill connection client after shutdown timeout".into(),
                    source,
                })?;
                child.wait().await.map_err(wait_error)
            }
        }
    }
}

fn terminate_child(child: &Child) -> Result<(), ClankerError> {
    let Some(id) = child.id() else {
        return Ok(());
    };
    let pid = i32::try_from(id).map_err(|_| {
        ClankerError::InvalidConfig("connection client process ID is not representable".into())
    })?;
    match kill(Pid::from_raw(pid), Signal::SIGTERM) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(ClankerError::Io {
            action: "terminate connection client".into(),
            source: std::io::Error::from_raw_os_error(error as i32),
        }),
    }
}

fn wait_error(source: std::io::Error) -> ClankerError {
    ClankerError::Io {
        action: "wait for connection client".into(),
        source,
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}
