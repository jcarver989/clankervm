#[cfg(test)]
#[path = "connect_tests.rs"]
mod tests;

use super::lifecycle::wait_until_running;
use crate::ClankerError;
use crate::OutputFormat;
use crate::application::{
    SelectedConnection, application_url, client_arguments, connection_settings, endpoint, failure,
    select,
};
use crate::client::{AuthTokenExpiration, MicroVmClient};
use crate::config::{ConnectionSettings, ProjectConfig};
use crate::output::render;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
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
    /// Print the application's URL and headers instead of running its local
    /// client, for a client that connects itself.
    #[arg(long, conflicts_with = "arguments")]
    pub print_endpoint: bool,
    #[arg(last = true, allow_hyphen_values = true)]
    pub arguments: Vec<String>,
}

/// Where a MicroVM's application is reachable, and the headers that open it.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApplicationEndpoint {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

impl ConnectOptions {
    /// Rejects an unknown connection, or a local client asked for JSON output,
    /// before credentials are touched.
    pub(super) fn check(
        &self,
        config: &ProjectConfig,
        format: OutputFormat,
    ) -> Result<(), ClankerError> {
        if self.print_endpoint {
            connection_settings(config, &self.name, self.connect_timeout).map(drop)
        } else {
            select(config, &self.name, self.connect_timeout, format).map(drop)
        }
    }
}

pub async fn connect<T: MicroVmClient>(
    options: ConnectOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    if options.print_endpoint {
        let settings = connection_settings(config, &options.name, options.connect_timeout)?;
        let endpoint = application_endpoint(client, &settings, &options.vm_id).await?;
        return render(format, &endpoint, || human_endpoint(&endpoint));
    }
    let selected = select(config, &options.name, options.connect_timeout, format)?;
    connect_to_vm(client, &selected, &options.vm_id, false, &options.arguments).await
}

/// The URL of a running MicroVM's application and the headers that reach it,
/// with a fresh token, for a client that connects itself.
pub(crate) async fn application_endpoint<T: MicroVmClient>(
    client: &T,
    settings: &ConnectionSettings,
    id: &str,
) -> Result<ApplicationEndpoint, ClankerError> {
    let deadline = Instant::now() + settings.timeout;
    let base = wait_for_endpoint(client, id, false, deadline).await?;
    let url = application_url(&base, &settings.path, settings.protocol)?;
    let port = settings.port.get();
    let token = timeout_at(
        deadline,
        client.create_auth_token(id, port, AUTH_TOKEN_EXPIRATION),
    )
    .await
    .map_err(|_| ClankerError::ApplicationOperationTimeout)??;
    Ok(ApplicationEndpoint {
        url: url.to_string(),
        headers: BTreeMap::from([
            ("X-aws-proxy-auth".to_owned(), token.value().to_owned()),
            ("X-aws-proxy-port".to_owned(), port.to_string()),
        ]),
    })
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

fn human_endpoint(endpoint: &ApplicationEndpoint) -> String {
    std::iter::once(endpoint.url.clone())
        .chain(
            endpoint
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}")),
        )
        .collect::<Vec<_>>()
        .join("\n")
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
    timeout_at(
        deadline,
        wait_until_running(client, id, launched, |details| {
            if details.endpoint.is_empty() {
                Ok(None)
            } else {
                endpoint(&details.endpoint).map(Some)
            }
        }),
    )
    .await
    .map_err(|_| ClankerError::ApplicationOperationTimeout)?
}
