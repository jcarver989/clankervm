#[cfg(test)]
#[path = "application_tests.rs"]
mod tests;

use crate::client::AuthToken;
use crate::config::{ConnectionSettings, ProjectConfig, Protocol};
use crate::{ClankerError, OutputFormat};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

pub(crate) fn failure(message: &str) -> ClankerError {
    ClankerError::Application(message.into())
}

pub(crate) fn validate_path(path: &str) -> Result<(), ClankerError> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\\', '#'])
        || path.chars().any(char::is_control)
    {
        return Err(ClankerError::InvalidConfig("connection paths must be same-origin absolute paths without backslashes, fragments or control characters".into()));
    }
    Ok(())
}

pub(crate) fn endpoint(value: &str) -> Result<Url, ClankerError> {
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    Url::parse(&value).map_err(|_| failure("AWS returned an invalid application endpoint"))
}

pub(crate) fn application_url(
    base: &Url,
    path: &str,
    protocol: Protocol,
) -> Result<Url, ClankerError> {
    validate_path(path)?;
    let mut url = base
        .join(path)
        .map_err(|_| failure("invalid application path"))?;
    if protocol == Protocol::Websocket {
        url.set_scheme("wss")
            .map_err(|()| failure("invalid WebSocket URL"))?;
    }
    Ok(url)
}

#[derive(Debug)]
pub(crate) struct SelectedConnection {
    pub settings: ConnectionSettings,
    pub executable: PathBuf,
}

pub(crate) fn select(
    config: &ProjectConfig,
    name: &str,
    timeout: Option<Duration>,
    format: OutputFormat,
) -> Result<SelectedConnection, ClankerError> {
    let mut settings = config
        .connections
        .get(name)
        .ok_or_else(|| ClankerError::InvalidConfig(format!("unknown connection `{name}`")))?
        .clone();
    if let Some(timeout) = timeout {
        settings.timeout = timeout;
    }
    if matches!(format, OutputFormat::Json) {
        return Err(ClankerError::InvalidConfig(
            "a connection with a local client cannot use --format json".into(),
        ));
    }
    let executable = Path::new(&settings.client[0]);
    let executable = if executable.components().count() == 1 {
        executable.to_owned() // Command::spawn resolves bare names using PATH.
    } else {
        config.resolve(executable)
    };
    Ok(SelectedConnection {
        settings,
        executable,
    })
}

pub(crate) fn client_arguments(
    template: &[String],
    url: &Url,
    token: &AuthToken,
    port: u16,
    trailing: &[String],
) -> Vec<String> {
    template
        .iter()
        .skip(1)
        .map(|arg| match arg.as_str() {
            "{url}" => url.to_string(),
            "{auth-header}" => format!("X-aws-proxy-auth: {}", token.value()),
            "{port-header}" => format!("X-aws-proxy-port: {port}"),
            _ => arg.clone(),
        })
        .chain(trailing.iter().cloned())
        .collect()
}
