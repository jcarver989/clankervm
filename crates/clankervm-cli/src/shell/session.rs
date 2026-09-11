//! A single `/shell` connection: binary terminal I/O and text control messages.

use super::socket::classify;
use super::terminal::{FALLBACK_SIZE, RawMode, TerminalSize};
use super::{Event, Outcome, ShellError, terminal};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Bytes, Error as WebSocketError, Message};

/// Time allowed for the authenticated upgrade and session preamble.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time allowed to finish the WebSocket close handshake.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// A stalled peer must not prevent the session from ending indefinitely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound queued output when the terminal cannot keep up with the remote PTY.
const OUTPUT_BUFFER: usize = 8;
/// Ctrl-] detaches locally rather than reaching the remote PTY.
const DETACH: u8 = 0x1d;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub size: TerminalSize,
    pub raw_mode: bool,
}

impl Options {
    pub(crate) fn local() -> Self {
        Self {
            size: terminal::size(),
            raw_mode: true,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            size: FALLBACK_SIZE,
            raw_mode: false,
        }
    }
}

/// Attaches until the shell exits, the local terminal closes, or Ctrl-] is typed.
/// The remote shell keeps its default TERM; no commands are injected into it.
pub(crate) async fn attach<T, U>(
    connection: impl Future<Output = Result<WebSocketStream<T>, ShellError>>,
    events: &mut mpsc::Receiver<Event>,
    out: &mut U,
    options: Options,
) -> Result<Outcome, ShellError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    U: AsyncWrite + Unpin,
{
    let socket = timeout(CONNECT_TIMEOUT, async {
        let mut socket = connection.await?;
        handshake(&mut socket).await?;
        Ok::<_, ShellError>(socket)
    })
    .await
    .map_err(|_| ShellError::Timeout("connecting to the MicroVM shell"))??;

    // Refused connections are reported before touching the local terminal.
    let _raw = options.raw_mode.then(RawMode::enable).transpose()?;
    pump(socket, events, out, options.size).await
}

async fn handshake<T>(socket: &mut WebSocketStream<T>) -> Result<(), ShellError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match socket.next().await.transpose().map_err(classify)? {
            Some(Message::Text(text)) => {
                let preamble = serde_json::from_str::<Value>(&text);
                return match preamble {
                    Ok(value) if value["type"] == "session_init" => Ok(()),
                    _ => Err(ShellError::Handshake),
                };
            }
            Some(Message::Ping(_)) => write(socket.flush()).await?,
            Some(Message::Pong(_)) => {}
            _ => return Err(ShellError::Handshake),
        }
    }
}

async fn pump<T, U>(
    mut socket: WebSocketStream<T>,
    events: &mut mpsc::Receiver<Event>,
    out: &mut U,
    size: TerminalSize,
) -> Result<Outcome, ShellError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    U: AsyncWrite + Unpin,
{
    let (sender, receiver) = mpsc::channel(OUTPUT_BUFFER);
    let output = render(receiver, out);
    tokio::pin!(output);
    let mut output_finished = false;
    let outcome = {
        let (mut sink, mut stream) = (&mut socket).split();
        tokio::select! {
            result = receive(&mut stream, sender) => result.map(|()| Outcome { detached: false }),
            result = transmit(&mut sink, events, size) => result,
            result = &mut output => {
                output_finished = true;
                result.map(|()| Outcome { detached: false })
            }
        }
    };
    let shutdown = async {
        if !matches!(outcome, Err(ShellError::Timeout(_))) {
            close(socket).await;
        }
    };
    let drain = async {
        if output_finished {
            return Ok(());
        }
        timeout(WRITE_TIMEOUT, output)
            .await
            .unwrap_or(Err(ShellError::Timeout("writing to the local terminal")))
    };
    let ((), drained) = tokio::join!(shutdown, drain);
    let outcome = outcome?;
    drained?;
    Ok(outcome)
}

async fn receive<T>(stream: &mut T, output: mpsc::Sender<Bytes>) -> Result<(), ShellError>
where
    T: Stream<Item = Result<Message, WebSocketError>> + Unpin,
{
    while let Some(frame) = stream.next().await {
        match frame.map_err(classify)? {
            Message::Binary(bytes) => {
                if output.send(bytes).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(())
}

/// Forwards local input and resizes until the terminal closes or Ctrl-] is typed.
async fn transmit<T>(
    sink: &mut T,
    events: &mut mpsc::Receiver<Event>,
    size: TerminalSize,
) -> Result<Outcome, ShellError>
where
    T: Sink<Message, Error = WebSocketError> + Unpin,
{
    write(sink.send(resize(size))).await?;
    while let Some(event) = events.recv().await {
        let (message, detach) = match event {
            Event::Input(mut bytes) => {
                let detach = bytes.iter().position(|byte| *byte == DETACH);
                if let Some(at) = detach {
                    bytes.truncate(at);
                }
                (Message::binary(bytes), detach.is_some())
            }
            Event::Resize(size) => (resize(size), false),
            Event::Eof | Event::Interrupt => break,
        };
        if !message.is_empty() {
            write(sink.send(message)).await?;
        }
        if detach {
            return Ok(Outcome { detached: true });
        }
    }
    Ok(Outcome { detached: false })
}

/// Writes queued output at whatever pace the terminal accepts it.
async fn render<T>(mut output: mpsc::Receiver<Bytes>, out: &mut T) -> Result<(), ShellError>
where
    T: AsyncWrite + Unpin,
{
    while let Some(bytes) = output.recv().await {
        out.write_all(&bytes).await?;
        out.flush().await?;
    }
    Ok(())
}

/// Bound sends and flushes so a stalled peer cannot hold the session open.
async fn write(
    operation: impl Future<Output = Result<(), WebSocketError>>,
) -> Result<(), ShellError> {
    timeout(WRITE_TIMEOUT, operation)
        .await
        .map_err(|_| ShellError::Timeout("writing to the MicroVM shell"))?
        .map_err(classify)
}

/// Native PTY resize control message on the same connection as terminal I/O.
fn resize(size: TerminalSize) -> Message {
    Message::text(json!({"type": "resize", "cols": size.cols, "rows": size.rows}).to_string())
}

/// Send/acknowledge close, then drive the handshake until EOF or the deadline.
async fn close<T>(mut socket: WebSocketStream<T>)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let _ = timeout(CLOSE_TIMEOUT, async {
        let _ = socket.close(None).await;
        while let Some(Ok(_)) = socket.next().await {}
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::socket::scripted::{Remote, scripted};
    use std::io;
    use tokio::io::{AsyncReadExt, DuplexStream, duplex};
    use tokio::time::Instant;

    async fn run<T: AsyncWrite + Unpin>(
        socket: WebSocketStream<DuplexStream>,
        events: &mut mpsc::Receiver<Event>,
        out: &mut T,
    ) -> Result<Outcome, ShellError> {
        attach(async { Ok(socket) }, events, out, Options::default()).await
    }

    async fn next(remote: &mut Remote) -> Message {
        timeout(CONNECT_TIMEOUT, remote.socket.next())
            .await
            .expect("client must send a frame")
            .expect("connection must remain open")
            .expect("valid WebSocket frame")
    }

    async fn expect_resize(remote: &mut Remote, rows: u16, cols: u16) {
        let Message::Text(text) = next(remote).await else {
            panic!("resize must be a text frame, not injected shell input");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"type": "resize", "rows": rows, "cols": cols})
        );
    }

    /// A terminal with no room left, so the next byte rendered to it blocks.
    async fn full_terminal() -> (DuplexStream, DuplexStream) {
        let (mut out, terminal) = duplex(1);
        out.write_all(b"!").await.unwrap();
        (out, terminal)
    }

    async fn expect_rendered(terminal: &mut DuplexStream, expected: &[u8]) {
        let mut rendered = vec![0; expected.len()];
        terminal.read_exact(&mut rendered).await.unwrap();
        assert_eq!(rendered, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn one_connection_sends_initial_and_live_resizes() {
        let (socket, mut remote) = scripted().await;
        let (sender, mut events) = mpsc::channel(8);
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 37, 123).await;
            for cols in [120, 130] {
                sender
                    .send(Event::Resize(TerminalSize { rows: 60, cols }))
                    .await
                    .unwrap();
                expect_resize(&mut remote, 60, cols).await;
            }
            remote.close().await;
        };
        let mut out = Vec::new();
        let (result, ()) = tokio::join!(
            attach(
                async { Ok(socket) },
                &mut events,
                &mut out,
                Options {
                    size: TerminalSize {
                        rows: 37,
                        cols: 123
                    },
                    raw_mode: false
                }
            ),
            script
        );
        assert_eq!(result.unwrap(), Outcome { detached: false });
        assert!(out.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn binary_io_is_verbatim_and_text_metadata_is_not_printed() {
        let (socket, mut remote) = scripted().await;
        let (sender, mut events) = mpsc::channel(8);
        let input = b"ls\r\x03\x00\xff\x1b[A";
        let output = b"\xff\x00\x1b[31mhello\r\n\x1e/dev/pts/7\x1f";
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            sender.send(Event::Input(input.to_vec())).await.unwrap();
            assert_eq!(next(&mut remote).await, Message::binary(input.to_vec()));
            remote
                .socket
                .send(Message::text(r#"{"type":"future_metadata"}"#))
                .await
                .unwrap();
            remote
                .socket
                .send(Message::binary(output[..5].to_vec()))
                .await
                .unwrap();
            remote
                .socket
                .send(Message::binary(output[5..].to_vec()))
                .await
                .unwrap();
            remote.close().await;
        };
        let mut out = Vec::new();
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert_eq!(result.unwrap(), Outcome { detached: false });
        assert_eq!(out, output);
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_bracket_sends_only_its_prefix_then_closes() {
        for input in [b"exit\r\x1dtail".as_slice(), b"\x1d"] {
            let (socket, mut remote) = scripted().await;
            let (sender, mut events) = mpsc::channel(8);
            let script = async {
                remote.init().await;
                expect_resize(&mut remote, 24, 80).await;
                sender.send(Event::Input(input.to_vec())).await.unwrap();
                if input[0] != DETACH {
                    assert_eq!(next(&mut remote).await, Message::binary("exit\r"));
                }
                assert!(matches!(next(&mut remote).await, Message::Close(_)));
                remote.socket.flush().await.unwrap();
            };
            let mut out = Vec::new();
            let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
            assert_eq!(result.unwrap(), Outcome { detached: true });
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ping_is_answered_during_handshake_and_when_terminal_is_idle() {
        let (socket, mut remote) = scripted().await;
        let (_sender, mut events) = mpsc::channel(8);
        let script = async {
            remote
                .socket
                .send(Message::Ping(b"before".as_slice().into()))
                .await
                .unwrap();
            assert_eq!(
                next(&mut remote).await,
                Message::Pong(b"before".as_slice().into())
            );
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            remote
                .socket
                .send(Message::Ping(b"after".as_slice().into()))
                .await
                .unwrap();
            assert_eq!(
                next(&mut remote).await,
                Message::Pong(b"after".as_slice().into())
            );
            remote.close().await;
        };
        let mut out = Vec::new();
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert!(result.is_ok());
        assert!(out.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_or_missing_preamble_is_rejected() {
        for frame in [
            Message::text("{}"),
            Message::text("not JSON"),
            Message::binary("prompt"),
            Message::Close(None),
        ] {
            let (socket, mut remote) = scripted().await;
            remote.socket.send(frame).await.unwrap();
            let (_sender, mut events) = mpsc::channel(8);
            let error = run(socket, &mut events, &mut Vec::new()).await.unwrap_err();
            assert!(matches!(error, ShellError::Handshake), "{error}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_session_init_is_bounded() {
        let (socket, _remote) = scripted().await;
        let (_sender, mut events) = mpsc::channel(8);
        let start = Instant::now();
        let error = run(socket, &mut events, &mut Vec::new()).await.unwrap_err();
        assert!(matches!(error, ShellError::Timeout(_)));
        assert_eq!(start.elapsed(), CONNECT_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn local_endings_close_with_a_bounded_wait_for_an_unresponsive_peer() {
        for event in [Some(Event::Eof), Some(Event::Interrupt), None] {
            let (socket, mut remote) = scripted().await;
            remote.init().await;
            let (sender, mut events) = mpsc::channel(8);
            if let Some(event) = event {
                sender.send(event).await.unwrap();
            }
            drop(sender);
            let start = Instant::now();
            let result = run(socket, &mut events, &mut Vec::new()).await.unwrap();
            assert_eq!(result, Outcome { detached: false });
            assert_eq!(start.elapsed(), CLOSE_TIMEOUT);
            expect_resize(&mut remote, 24, 80).await;
            assert!(matches!(next(&mut remote).await, Message::Close(_)));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn abrupt_transport_failure_is_reported() {
        let (socket, mut remote) = scripted().await;
        let (_sender, mut events) = mpsc::channel(8);
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            drop(remote);
        };
        let mut out = Vec::new();
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert!(matches!(result, Err(ShellError::Connect(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_connection_future_is_bounded() {
        let (_sender, mut events) = mpsc::channel(8);
        let start = Instant::now();
        let result = attach(
            std::future::pending::<Result<WebSocketStream<DuplexStream>, ShellError>>(),
            &mut events,
            &mut Vec::new(),
            Options::default(),
        )
        .await;
        assert!(matches!(result, Err(ShellError::Timeout(_))));
        assert_eq!(start.elapsed(), CONNECT_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_resize_and_handshake_pong_have_write_deadlines() {
        for preamble in [
            Message::text(r#"{"type":"session_init"}"#),
            Message::Ping(vec![1].into()),
        ] {
            let (socket, mut remote) = Remote::pair(1).await;
            let (_sender, mut events) = mpsc::channel(8);
            let mut out = Vec::new();
            let start = Instant::now();
            let (result, sent) = tokio::join!(
                run(socket, &mut events, &mut out),
                remote.socket.send(preamble),
            );
            sent.unwrap();
            assert!(matches!(
                result,
                Err(ShellError::Timeout("writing to the MicroVM shell"))
            ));
            // No close attempt or retry after a partially written frame.
            assert_eq!(start.elapsed(), WRITE_TIMEOUT);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn input_and_resize_backpressure_end_the_session() {
        for event in [
            Event::Input(vec![b'x'; 128]),
            Event::Resize(TerminalSize {
                rows: 37,
                cols: 123,
            }),
        ] {
            let (socket, mut remote) = Remote::pair(1).await;
            let (sender, mut events) = mpsc::channel(8);
            let script = async {
                remote.init().await;
                expect_resize(&mut remote, 24, 80).await;
                sender.send(event).await.unwrap();
                // Keep the peer alive but stop reading its socket.
            };
            let mut out = Vec::new();
            let start = Instant::now();
            let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
            assert!(matches!(
                result,
                Err(ShellError::Timeout("writing to the MicroVM shell"))
            ));
            assert_eq!(start.elapsed(), WRITE_TIMEOUT);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_terminal_output_does_not_delay_input_resizes_or_pongs() {
        let (socket, mut remote) = scripted().await;
        let (sender, mut events) = mpsc::channel(8);
        let (mut out, mut terminal) = full_terminal().await;
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            remote.socket.send(Message::binary("first")).await.unwrap();
            remote.socket.send(Message::binary("second")).await.unwrap();
            remote
                .socket
                .send(Message::Ping(b"ping".as_slice().into()))
                .await
                .unwrap();
            // Seeing the pong proves the reader passed the output frames while
            // the terminal still has no capacity to render them.
            assert_eq!(
                next(&mut remote).await,
                Message::Pong(b"ping".as_slice().into())
            );
            sender.send(Event::Input(b"typed".to_vec())).await.unwrap();
            assert_eq!(next(&mut remote).await, Message::binary("typed"));
            sender
                .send(Event::Resize(TerminalSize {
                    rows: 37,
                    cols: 123,
                }))
                .await
                .unwrap();
            expect_resize(&mut remote, 37, 123).await;
            sender.send(Event::Input(vec![DETACH])).await.unwrap();
            assert!(matches!(next(&mut remote).await, Message::Close(_)));
            remote.socket.flush().await.unwrap();
            expect_rendered(&mut terminal, b"!firstsecond").await;
        };
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert_eq!(result.unwrap(), Outcome { detached: true });
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_socket_input_does_not_delay_terminal_output() {
        let (socket, mut remote) = Remote::pair(1).await;
        let (sender, mut events) = mpsc::channel(8);
        let (mut out, mut terminal) = duplex(1);
        let input = vec![b'x'; 128];
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            sender.send(Event::Input(input.clone())).await.unwrap();
            // Do not read the input frame until output has reached the terminal.
            remote.socket.send(Message::binary("output")).await.unwrap();
            expect_rendered(&mut terminal, b"output").await;
            assert_eq!(next(&mut remote).await, Message::binary(input));
            remote.close().await;
        };
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert_eq!(result.unwrap(), Outcome { detached: false });
    }

    #[tokio::test(start_paused = true)]
    async fn remote_close_during_a_blocked_input_send_has_a_bounded_close() {
        let (socket, mut remote) = Remote::pair(1).await;
        let (sender, mut events) = mpsc::channel(8);
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            sender.send(Event::Input(vec![b'x'; 128])).await.unwrap();
            // Let the input send fill the transport before the peer closes.
            tokio::task::yield_now().await;
            remote.close().await;
        };
        let mut out = Vec::new();
        let start = Instant::now();
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert_eq!(result.unwrap(), Outcome { detached: false });
        // The close handshake waits on the same unread transport, briefly.
        assert_eq!(start.elapsed(), CLOSE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_terminal_does_not_block_detaching() {
        let (socket, mut remote) = scripted().await;
        let (sender, mut events) = mpsc::channel(8);
        let (mut out, _terminal) = full_terminal().await;
        let start = Instant::now();
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            remote.socket.send(Message::binary("output")).await.unwrap();
            // The pong proves the output frame is queued for a terminal that
            // will never accept it.
            remote
                .socket
                .send(Message::Ping(b"ping".as_slice().into()))
                .await
                .unwrap();
            assert_eq!(
                next(&mut remote).await,
                Message::Pong(b"ping".as_slice().into())
            );
            sender.send(Event::Input(vec![DETACH])).await.unwrap();
            // The close is not held up by the terminal; only the final drain
            // has a deadline.
            assert!(matches!(next(&mut remote).await, Message::Close(_)));
            assert_eq!(start.elapsed(), Duration::ZERO);
            remote.socket.flush().await.unwrap();
        };
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert!(matches!(
            result,
            Err(ShellError::Timeout("writing to the local terminal"))
        ));
        assert_eq!(start.elapsed(), WRITE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn output_failure_is_reported_and_closes_the_socket() {
        let (socket, mut remote) = scripted().await;
        let (_sender, mut events) = mpsc::channel(8);
        let script = async {
            remote.init().await;
            expect_resize(&mut remote, 24, 80).await;
            remote.socket.send(Message::binary("output")).await.unwrap();
            assert!(matches!(next(&mut remote).await, Message::Close(_)));
            remote.socket.flush().await.unwrap();
        };
        // A real fixed-capacity writer with no room for output.
        let mut out = io::Cursor::new(&mut [][..]);
        let (result, ()) = tokio::join!(run(socket, &mut events, &mut out), script);
        assert!(matches!(result, Err(ShellError::Terminal(_))));
    }
}
