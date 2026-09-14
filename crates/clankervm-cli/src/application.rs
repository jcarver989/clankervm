use crate::config::{ConnectionProtocol, ConnectionSettings, ProjectConfig};
use crate::{ClankerError, OutputFormat};
use reqwest::header::{HeaderMap, HeaderValue};
use std::path::{Component, Path, PathBuf};
use url::Url;

pub(crate) const AUTH_HEADER: &str = "X-aws-proxy-auth";
pub(crate) const PORT_HEADER: &str = "X-aws-proxy-port";
const PLACEHOLDERS: [&str; 3] = ["{url}", "{auth-header}", "{port-header}"];

pub(crate) fn validate_path(path: &str, field: &str) -> Result<(), ClankerError> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return Err(ClankerError::InvalidConfig(format!(
            "{field} must be a same-origin absolute path"
        )));
    }
    let parsed = Url::parse(&format!("https://path.invalid{path}")).map_err(|_| {
        ClankerError::InvalidConfig(format!("{field} is not a valid absolute path"))
    })?;
    if parsed.host_str() != Some("path.invalid")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ClankerError::InvalidConfig(format!(
            "{field} must be a same-origin absolute path without a fragment"
        )));
    }
    Ok(())
}

pub(crate) fn validate_client_template(
    client: &[String],
    prefix: &str,
) -> Result<(), ClankerError> {
    if client
        .first()
        .is_some_and(|value| PLACEHOLDERS.contains(&value.as_str()))
    {
        return Err(ClankerError::InvalidConfig(format!(
            "{prefix}.client executable cannot be a placeholder"
        )));
    }
    for argument in client {
        if (argument.contains('{') || argument.contains('}'))
            && !PLACEHOLDERS.contains(&argument.as_str())
        {
            return Err(ClankerError::InvalidConfig(format!(
                "{prefix}.client placeholders must occupy a whole argument"
            )));
        }
    }
    Ok(())
}

pub(crate) fn preflight_client(
    config: &ProjectConfig,
    settings: &ConnectionSettings,
    format: OutputFormat,
) -> Result<Option<PathBuf>, ClankerError> {
    let Some(client) = &settings.client else {
        return Ok(None);
    };
    if matches!(format, OutputFormat::Json) {
        return Err(ClankerError::InvalidConfig(
            "a configured connection client cannot be combined with --format json".into(),
        ));
    }
    let executable = Path::new(&client[0]);
    let resolved = if executable.is_absolute() {
        executable.to_owned()
    } else if executable.components().count() > 1
        || matches!(
            executable.components().next(),
            Some(Component::CurDir | Component::ParentDir)
        )
    {
        config.resolve(executable)
    } else {
        which::which(executable).map_err(|_| {
            ClankerError::InvalidConfig(format!(
                "connection client executable `{}` was not found in PATH",
                client[0]
            ))
        })?
    };
    if !is_executable(&resolved) {
        return Err(ClankerError::InvalidConfig(format!(
            "connection client executable `{}` does not exist or is not executable",
            resolved.display()
        )));
    }
    Ok(Some(resolved))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub(crate) fn endpoint_url(
    endpoint: &str,
    region: &str,
    path: &str,
    protocol: ConnectionProtocol,
) -> Result<Url, ClankerError> {
    let base = Url::parse(endpoint).map_err(|_| ClankerError::UnsafeEndpoint)?;
    let expected_suffix = format!(".lambda-microvm.{region}.on.aws");
    let host = base.host_str().ok_or(ClankerError::UnsafeEndpoint)?;
    let label = host
        .strip_suffix(&expected_suffix)
        .ok_or(ClankerError::UnsafeEndpoint)?;
    let valid_label = !label.is_empty()
        && !label.contains('.')
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if base.scheme() != "https"
        || !valid_label
        || base.port_or_known_default() != Some(443)
        || base.port().is_some_and(|port| port != 443)
        || base.username() != ""
        || base.password().is_some()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(ClankerError::UnsafeEndpoint);
    }
    let mut application = base.join(path).map_err(|_| ClankerError::UnsafeEndpoint)?;
    if matches!(protocol, ConnectionProtocol::Websocket) {
        application
            .set_scheme("wss")
            .map_err(|()| ClankerError::UnsafeEndpoint)?;
    }
    Ok(application)
}

pub(crate) struct PreparedApplication {
    pub url: Url,
    pub headers: HeaderMap,
}

pub(crate) fn prepare(
    endpoint: &str,
    region: &str,
    path: &str,
    protocol: ConnectionProtocol,
    port: u16,
    token: &str,
) -> Result<PreparedApplication, ClankerError> {
    let url = endpoint_url(endpoint, region, path, protocol)?;
    let mut auth =
        HeaderValue::from_str(token).map_err(|_| ClankerError::InvalidApplicationToken)?;
    auth.set_sensitive(true);
    let port = HeaderValue::from_str(&port.to_string())
        .map_err(|_| ClankerError::InvalidApplicationToken)?;
    let mut headers = HeaderMap::new();
    headers.insert(AUTH_HEADER, auth);
    headers.insert(PORT_HEADER, port);
    Ok(PreparedApplication { url, headers })
}

pub(crate) fn client_arguments(
    settings: &ConnectionSettings,
    executable: &Path,
    prepared: &PreparedApplication,
    trailing: &[String],
) -> Vec<String> {
    let auth = format!(
        "{AUTH_HEADER}: {}",
        prepared.headers[AUTH_HEADER]
            .to_str()
            .expect("validated auth header")
    );
    let port = format!("{PORT_HEADER}: {}", settings.port);
    let url = prepared.url.as_str();
    let mut arguments = vec![executable.to_string_lossy().into_owned()];
    arguments.extend(
        settings
            .client
            .as_ref()
            .expect("client is configured")
            .iter()
            .skip(1)
            .map(|argument| match argument.as_str() {
                "{url}" => url.to_owned(),
                "{auth-header}" => auth.clone(),
                "{port-header}" => port.clone(),
                _ => argument.clone(),
            }),
    );
    arguments.extend_from_slice(trailing);
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_region_scoped_and_keeps_public_tls_port() {
        let url = endpoint_url(
            "https://vm-123.lambda-microvm.us-east-1.on.aws",
            "us-east-1",
            "/healthz",
            ConnectionProtocol::Http,
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://vm-123.lambda-microvm.us-east-1.on.aws/healthz"
        );
        assert_eq!(url.port_or_known_default(), Some(443));
        for endpoint in [
            "http://vm-123.lambda-microvm.us-east-1.on.aws/",
            "https://vm-123.lambda-microvm.eu-west-1.on.aws/",
            "https://evil.vm-123.lambda-microvm.us-east-1.on.aws/",
            "https://vm-123.lambda-microvm.us-east-1.on.aws/path",
            "https://user@vm-123.lambda-microvm.us-east-1.on.aws/",
        ] {
            assert!(
                endpoint_url(endpoint, "us-east-1", "/", ConnectionProtocol::Http).is_err(),
                "accepted {endpoint}"
            );
        }
    }

    #[test]
    fn configured_paths_and_templates_are_same_origin_and_whole_token() {
        for path in ["", "relative", "//evil.test", "/bad\\path", "/line\nfeed"] {
            assert!(validate_path(path, "path").is_err(), "accepted {path:?}");
        }
        assert!(validate_path("/api/info?full=true", "path").is_ok());
        assert!(
            validate_client_template(&["curl".into(), "prefix-{url}".into()], "connection")
                .is_err()
        );
        assert!(validate_client_template(&["{url}".into()], "connection").is_err());
        assert!(
            validate_client_template(
                &["curl".into(), "{url}".into(), "{auth-header}".into()],
                "connection"
            )
            .is_ok()
        );
    }
}
