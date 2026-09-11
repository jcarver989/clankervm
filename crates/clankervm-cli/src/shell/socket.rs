//! Opening `/shell` connections: the URL, the authenticated upgrade, and the
//! in-memory stand-in the tests drive a session over.

use super::ShellError;
use crate::client::ShellToken;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::{StatusCode, Uri};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

/// The path AWS exposes a MicroVM's pty on.
const SHELL_PATH: &str = "/shell";

/// TLS and the authenticated WebSocket upgrade.
pub(crate) async fn connect(
    request: ClientRequestBuilder,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, ShellError> {
    let (stream, _) = connect_async_with_config(request, None, true)
        .await
        .map_err(classify)?;
    Ok(stream)
}

/// The validated shell URL and all authentication/routing headers AWS returned.
pub(crate) fn request(
    endpoint: &str,
    token: &ShellToken,
) -> Result<ClientRequestBuilder, ShellError> {
    Ok(token.headers.iter().fold(
        ClientRequestBuilder::new(shell_url(endpoint)?),
        |request, (name, value)| request.with_header(name, value),
    ))
}

/// Maps a websocket failure onto the session's errors. A refusal at the upgrade
/// is how AWS reports a token that is expired or belongs to another MicroVM.
///
/// A websocket error carries the response, never the request, so the token
/// cannot leak into a message.
pub(super) fn classify(error: WebSocketError) -> ShellError {
    match error {
        WebSocketError::Http(response) if response.status() == StatusCode::FORBIDDEN => {
            ShellError::Unauthorized
        }
        error => ShellError::Connect(error.to_string()),
    }
}

/// The `wss://<host>/shell` URL of a MicroVM endpoint.
///
/// Accepts a bare host, with or without a port, or the `https://` URL AWS
/// reports; a `wss://` URL is accepted too, so an endpoint can be pasted from
/// either place.
pub(crate) fn shell_url(endpoint: &str) -> Result<Uri, ShellError> {
    let endpoint = endpoint.trim();
    let host = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("wss://"))
        .unwrap_or(endpoint)
        .trim_end_matches('/');
    let usable = !host.is_empty()
        && !host
            .chars()
            .any(|character| matches!(character, '@' | '?' | '#' | '/'));
    if !usable {
        return Err(ShellError::InvalidEndpoint(endpoint.to_owned()));
    }
    format!("wss://{host}{SHELL_PATH}")
        .parse()
        .map_err(|_| ShellError::InvalidEndpoint(endpoint.to_owned()))
}

/// Real in-memory WebSocket peers for session and lifecycle tests.
#[cfg(test)]
pub(crate) mod scripted {
    use futures_util::SinkExt;
    use tokio::io::{DuplexStream, duplex};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// Room for everything a test session sends before its script reads it.
    const BUFFER: usize = 64 * 1024;
    /// The text frame AWS sends before a connection carries bytes.
    const SESSION_INIT: &str = r#"{"type":"session_init"}"#;

    /// The MicroVM's end of the same pipe.
    pub(crate) struct Remote {
        pub socket: WebSocketStream<DuplexStream>,
    }

    pub(crate) async fn scripted() -> (WebSocketStream<DuplexStream>, Remote) {
        Remote::pair(BUFFER).await
    }

    impl Remote {
        /// A capacity small enough to fill lets tests exercise backpressure.
        pub(crate) async fn pair(capacity: usize) -> (WebSocketStream<DuplexStream>, Self) {
            let (local, remote) = duplex(capacity);
            (
                WebSocketStream::from_raw_socket(local, Role::Client, None).await,
                Self {
                    socket: WebSocketStream::from_raw_socket(remote, Role::Server, None).await,
                },
            )
        }

        /// Sends AWS's session preamble.
        pub(crate) async fn init(&mut self) {
            self.socket.send(Message::text(SESSION_INIT)).await.unwrap();
        }

        /// Closes the connection from the MicroVM's side.
        pub(crate) async fn close(&mut self) {
            let _ = self.socket.close(None).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    #[tokio::test]
    async fn shell_connections_disable_nagle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/shell", listener.local_addr().unwrap());
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(socket).await.unwrap()
        };
        let (client, _server) = tokio::join!(
            connect(ClientRequestBuilder::new(url.parse().unwrap())),
            server
        );
        let client = client.unwrap();
        let MaybeTlsStream::Plain(socket) = client.get_ref() else {
            panic!("loopback connection should not use TLS");
        };
        assert!(socket.nodelay().unwrap());
    }

    #[test]
    fn an_endpoint_becomes_the_shell_url() {
        for endpoint in [
            "example.test",
            "https://example.test",
            "https://example.test/",
            "wss://example.test",
            "  example.test  ",
        ] {
            assert_eq!(
                shell_url(endpoint).unwrap().to_string(),
                "wss://example.test/shell",
                "rejected {endpoint:?}"
            );
        }

        assert_eq!(
            shell_url("example.test:8443").unwrap().to_string(),
            "wss://example.test:8443/shell"
        );
    }

    #[test]
    fn an_endpoint_that_is_not_a_host_is_rejected() {
        for endpoint in [
            "",
            "   ",
            "http://example.test",
            "user@example.test",
            "example.test/shell",
            "example.test?token=x",
        ] {
            assert!(
                matches!(shell_url(endpoint), Err(ShellError::InvalidEndpoint(_))),
                "accepted {endpoint:?}"
            );
        }
    }

    #[test]
    fn the_token_becomes_the_upgrade_headers() {
        let token = ShellToken {
            headers: HashMap::from([("X-aws-proxy-auth".to_owned(), "secret".to_owned())]),
        };

        let request = request("example.test", &token)
            .unwrap()
            .into_client_request()
            .unwrap();

        assert_eq!(request.uri().to_string(), "wss://example.test/shell");
        assert_eq!(request.headers()["X-aws-proxy-auth"], "secret");
    }
}
