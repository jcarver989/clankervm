#[cfg(test)]
#[path = "connect_tests.rs"]
mod tests;

use crate::ClankerError;
use crate::OutputFormat;
use crate::application::{
    SelectedConnection, application_url, client_arguments, endpoint, failure, select,
};
use crate::client::{AuthTokenExpiration, MicroVmClient};
use crate::config::ProjectConfig;
use aws_sdk_lambdamicrovms::types::MicrovmState;
use clap::Args;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

const AUTH_TOKEN_EXPIRATION: AuthTokenExpiration =
    AuthTokenExpiration::from_minutes(60).expect("60 minutes is a valid token expiration");

#[derive(Debug, Args)]
pub struct ConnectOptions {
    pub vm_id: String,
    pub name: String,
    #[arg(long, value_parser = humantime::parse_duration)]
    pub connect_timeout: Option<Duration>,
    #[arg(last = true, allow_hyphen_values = true)]
    pub arguments: Vec<String>,
}

pub async fn connect<T: MicroVmClient>(
    options: ConnectOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let selected = select(config, &options.name, options.connect_timeout, format)?;
    connect_to_vm(client, &selected, &options.vm_id, false, &options.arguments).await
}

pub(crate) async fn connect_to_vm<T: MicroVmClient>(
    client: &T,
    selected: &SelectedConnection,
    id: &str,
    launched: bool,
    arguments: &[String],
) -> Result<(), ClankerError> {
    let deadline = Instant::now() + selected.settings.timeout;
    let base = wait_for_endpoint(client, id, launched, deadline).await?;
    let url = application_url(&base, &selected.settings.path, selected.settings.protocol)?;
    let port = selected.settings.port.get();
    let mut delay = Duration::from_secs(1);

    loop {
        let token = timeout_at(
            deadline,
            client.create_auth_token(id, port, AUTH_TOKEN_EXPIRATION),
        )
        .await
        .map_err(|_| ClankerError::ApplicationOperationTimeout)??;

        let status = Command::new(&selected.executable)
            .args(client_arguments(
                &selected.settings.client,
                &url,
                &token,
                port,
                arguments,
            ))
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|_| failure("failed to run local client"))?;

        if status.success() {
            return Ok(());
        }

        let code = exit_code(status);

        if Instant::now() >= deadline {
            return Err(ClankerError::ClientExit(code));
        }

        eprintln!(
            "Local client exited with status {code}; retrying in {}s",
            delay.as_secs()
        );

        if timeout_at(deadline, sleep(delay)).await.is_err() {
            return Err(ClankerError::ClientExit(code));
        }

        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

fn exit_code(status: ExitStatus) -> u8 {
    u8::try_from(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    )
    .unwrap_or(1)
}

async fn wait_for_endpoint<T: MicroVmClient>(
    client: &T,
    id: &str,
    launched: bool,
    deadline: Instant,
) -> Result<Url, ClankerError> {
    timeout_at(deadline, async {
        let mut observed = false;
        loop {
            let Some(details) = client.get_details(id).await? else {
                if !launched || observed {
                    return Err(ClankerError::MicroVmNotFound(id.into()));
                }
                sleep(Duration::from_secs(1)).await;
                continue;
            };

            observed = true;

            match details.state {
                MicrovmState::Pending => {
                    sleep(Duration::from_secs(1)).await;
                }
                MicrovmState::Running if details.endpoint.is_empty() => {
                    sleep(Duration::from_secs(1)).await;
                }

                MicrovmState::Running => return endpoint(&details.endpoint),
                state => {
                    return Err(ClankerError::MicroVmNotRunning {
                        microvm_id: id.into(),
                        state: state.to_string(),
                        reason: details.state_reason,
                    });
                }
            }
        }
    })
    .await
    .map_err(|_| ClankerError::ApplicationOperationTimeout)?
}
