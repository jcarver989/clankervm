use crate::ClankerError;
use crate::application::{PreparedApplication, prepare};
use crate::arn::Arn;
use crate::client::{ALL_INGRESS, MicroVmClient};
use crate::config::{ConnectionProtocol, ConnectionSettings};
use aws_sdk_lambdamicrovms::types::MicrovmState;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

trait ApplicationProbe {
    fn probe<'a>(
        &'a self,
        prepared: &'a PreparedApplication,
        settings: &'a ConnectionSettings,
    ) -> impl Future<Output = Result<(), ProbeError>> + 'a;
}

struct NetworkProbe;

impl ApplicationProbe for NetworkProbe {
    fn probe<'a>(
        &'a self,
        prepared: &'a PreparedApplication,
        settings: &'a ConnectionSettings,
    ) -> impl Future<Output = Result<(), ProbeError>> + 'a {
        probe(prepared, settings)
    }
}

pub(crate) async fn wait_for_application<T: MicroVmClient>(
    client: &T,
    microvm_id: &str,
    settings: &ConnectionSettings,
    region: &str,
    launched: bool,
    duration: Duration,
) -> Result<PreparedApplication, ClankerError> {
    wait_for_application_with_probe(
        client,
        microvm_id,
        settings,
        region,
        launched,
        duration,
        &NetworkProbe,
    )
    .await
}

async fn wait_for_application_with_probe<T: MicroVmClient, P: ApplicationProbe>(
    client: &T,
    microvm_id: &str,
    settings: &ConnectionSettings,
    region: &str,
    launched: bool,
    duration: Duration,
    probe: &P,
) -> Result<PreparedApplication, ClankerError> {
    let deadline = Instant::now() + duration;
    let mut attempt = 0u32;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClankerError::ApplicationWaitTimeout {
                microvm_id: microvm_id.into(),
                timeout: duration,
            });
        }
        let control_timeout = remaining.min(CONTROL_TIMEOUT);
        let details = timeout(control_timeout, client.describe(microvm_id))
            .await
            .map_err(|_| ClankerError::ApplicationWaitTimeout {
                microvm_id: microvm_id.into(),
                timeout: duration,
            })??;
        let Some(details) = details else {
            if launched {
                backoff(deadline, attempt).await;
                attempt += 1;
                continue;
            }
            return Err(ClankerError::MicroVmNotFound(microvm_id.into()));
        };
        if details.state == MicrovmState::Pending {
            backoff(deadline, attempt).await;
            attempt += 1;
            continue;
        }
        if details.state != MicrovmState::Running {
            return Err(ClankerError::MicroVmNotRunning {
                microvm_id: microvm_id.into(),
                state: details.state.to_string(),
                reason: details.state_reason,
            });
        }
        ensure_application_ingress(&details.ingress_network_connectors, microvm_id, region)?;
        if details.endpoint.is_empty() {
            backoff(deadline, attempt).await;
            attempt += 1;
            continue;
        }
        let token = timeout(
            control_timeout.min(deadline.saturating_duration_since(Instant::now())),
            client.application_token(microvm_id, settings.port),
        )
        .await
        .map_err(|_| ClankerError::ApplicationWaitTimeout {
            microvm_id: microvm_id.into(),
            timeout: duration,
        })??;
        let prepared = prepare(
            &details.endpoint,
            region,
            settings.readiness_path(),
            settings.protocol,
            settings.port,
            token.expose(),
        )?;
        match timeout(
            PROBE_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())),
            probe.probe(&prepared, settings),
        )
        .await
        {
            Ok(Ok(())) => return Ok(prepared),
            Ok(Err(ProbeError::Retry)) | Err(_) => {
                backoff(deadline, attempt).await;
                attempt += 1;
            }
            Ok(Err(ProbeError::Fatal(message))) => {
                return Err(ClankerError::ApplicationProbe(message));
            }
        }
    }
}

pub(crate) fn ensure_application_ingress(
    connectors: &[String],
    microvm_id: &str,
    region: &str,
) -> Result<(), ClankerError> {
    let expected = Arn::network_connector(region, ALL_INGRESS)?;
    if connectors
        .iter()
        .any(|connector| connector == expected.as_str())
    {
        return Ok(());
    }
    Err(ClankerError::InvalidConfig(format!(
        "MicroVM `{microvm_id}` was not launched with {ALL_INGRESS}; application connections require ALL_INGRESS"
    )))
}

async fn backoff(deadline: Instant, attempt: u32) {
    let seconds = match attempt {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 5,
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    sleep(Duration::from_secs(seconds).min(remaining)).await;
}

enum ProbeError {
    Retry,
    Fatal(String),
}

async fn probe(
    prepared: &PreparedApplication,
    settings: &ConnectionSettings,
) -> Result<(), ProbeError> {
    match settings.protocol {
        ConnectionProtocol::Http => probe_http(prepared, settings).await,
        ConnectionProtocol::Websocket => probe_websocket(prepared, settings).await,
    }
}

async fn probe_http(
    prepared: &PreparedApplication,
    settings: &ConnectionSettings,
) -> Result<(), ProbeError> {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ProbeError::Fatal("failed to prepare application readiness request".into()))?
        .get(prepared.url.clone())
        .headers(prepared.headers.clone())
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() || error.is_connect() {
                ProbeError::Retry
            } else {
                ProbeError::Fatal("application readiness request failed".into())
            }
        })?;
    let status = response.status().as_u16();
    if status == settings.expected_status() {
        return Ok(());
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return Err(ProbeError::Retry);
    }
    if let Some(message) = settings.readiness.error_messages.get(&status) {
        return Err(ProbeError::Fatal(message.clone()));
    }
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Err(ProbeError::Fatal(format!(
            "application readiness endpoint redirected with HTTP {status}"
        )));
    }
    if matches!(status, 401 | 403) {
        return Err(ProbeError::Fatal(format!(
            "application authentication failed with HTTP {status}"
        )));
    }
    Err(ProbeError::Fatal(format!(
        "application readiness returned unexpected HTTP {status}"
    )))
}

async fn probe_websocket(
    prepared: &PreparedApplication,
    settings: &ConnectionSettings,
) -> Result<(), ProbeError> {
    let mut request = prepared
        .url
        .as_str()
        .into_client_request()
        .map_err(|_| ProbeError::Fatal("invalid WebSocket readiness request".into()))?;
    for (name, value) in &prepared.headers {
        request.headers_mut().insert(name, value.clone());
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| {
            if let tokio_tungstenite::tungstenite::Error::Http(response) = &error {
                let status = response.status().as_u16();
                if matches!(status, 429 | 500 | 502 | 503 | 504) {
                    return ProbeError::Retry;
                }
                if let Some(message) = settings.readiness.error_messages.get(&status) {
                    return ProbeError::Fatal(message.clone());
                }
                return ProbeError::Fatal(format!(
                    "WebSocket readiness upgrade failed with HTTP {status}"
                ));
            }
            match error {
                tokio_tungstenite::tungstenite::Error::Io(_)
                | tokio_tungstenite::tungstenite::Error::ConnectionClosed
                | tokio_tungstenite::tungstenite::Error::AlreadyClosed => ProbeError::Retry,
                tokio_tungstenite::tungstenite::Error::Tls(_) => {
                    ProbeError::Fatal("WebSocket TLS validation failed".into())
                }
                _ => ProbeError::Fatal("malformed WebSocket readiness response".into()),
            }
        })?;
    socket
        .send(Message::Close(None))
        .await
        .map_err(|_| ProbeError::Fatal("WebSocket readiness probe could not send Close".into()))?;
    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(_) => {
                return Err(ProbeError::Fatal(
                    "WebSocket readiness probe did not close cleanly".into(),
                ));
            }
        }
    }
    Err(ProbeError::Fatal(
        "WebSocket readiness probe closed without a closing handshake".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ApplicationToken, Call, FakeMicroVmClient};
    use crate::config::ReadinessSettings;
    use crate::test_support::MicroVmDetailsBuilder;
    use std::collections::BTreeMap;
    use std::future::ready;

    const ALL_INGRESS_ARN: &str =
        "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS";

    struct SuccessfulProbe;

    impl ApplicationProbe for SuccessfulProbe {
        fn probe<'a>(
            &'a self,
            _prepared: &'a PreparedApplication,
            _settings: &'a ConnectionSettings,
        ) -> impl Future<Output = Result<(), ProbeError>> + 'a {
            ready(Ok(()))
        }
    }

    fn settings() -> ConnectionSettings {
        ConnectionSettings {
            port: 3000,
            protocol: ConnectionProtocol::Http,
            path: "/api".into(),
            timeout: Duration::from_secs(10),
            client: None,
            readiness: ReadinessSettings {
                path: Some("/healthz".into()),
                expected_status: Some(204),
                error_messages: BTreeMap::new(),
            },
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_existing_pending_microvm_is_polled_until_running() {
        let pending = MicroVmDetailsBuilder::new("vm-1")
            .state(MicrovmState::Pending)
            .build();
        let running = MicroVmDetailsBuilder::new("vm-1")
            .endpoint("https://vm-1.lambda-microvm.us-east-1.on.aws")
            .ingress_network_connectors([ALL_INGRESS_ARN])
            .build();
        let client = FakeMicroVmClient::default()
            .described([Ok(Some(pending)), Ok(Some(running))])
            .application_tokens([ApplicationToken::new("header.safe-token".into())]);

        let ready = wait_for_application_with_probe(
            &client,
            "vm-1",
            &settings(),
            "us-east-1",
            false,
            Duration::from_secs(10),
            &SuccessfulProbe,
        )
        .await
        .unwrap();

        assert_eq!(ready.url.path(), "/healthz");
        assert_eq!(
            client.calls(),
            [
                Call::Describe("vm-1".into()),
                Call::Describe("vm-1".into()),
                Call::ApplicationToken("vm-1".into(), 3000),
            ]
        );
    }
}
