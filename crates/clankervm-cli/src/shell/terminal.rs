//! Local terminal state: raw mode, sizes, and the events a session reacts to.

use super::ShellError;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size as terminal_size};
use crossterm::tty::IsTty;
use std::io::Read;
use std::thread;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

/// The rows and columns assumed when the terminal reports neither.
pub(super) const FALLBACK_SIZE: TerminalSize = TerminalSize { rows: 24, cols: 80 };
/// Events the session may fall behind by before interrupting the reader.
const EVENT_BUFFER: usize = 64;
/// Bytes read from the terminal at a time.
const INPUT_CHUNK: usize = 4096;

/// Named dimensions avoid mixing crossterm's column-first order with PTY rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl From<(u16, u16)> for TerminalSize {
    fn from((cols, rows): (u16, u16)) -> Self {
        Self { rows, cols }
    }
}

/// One thing a session has to react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Bytes typed at the local terminal, forwarded to the remote pty verbatim.
    Input(Vec<u8>),
    /// Standard input reached its end, so the session is over.
    Eof,
    /// The local terminal changed size.
    Resize(TerminalSize),
    /// The user asked the process to stop.
    Interrupt,
}

/// Raw mode, restored when the guard drops, however the session ends.
pub(crate) struct RawMode;

impl RawMode {
    /// Switches the local terminal to raw mode so every keystroke reaches the
    /// remote pty as a byte, including the ones that are normally signals.
    pub(crate) fn enable() -> Result<Self, ShellError> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Whether both standard input and standard output are terminals.
pub(crate) fn is_interactive() -> bool {
    std::io::stdin().is_tty() && std::io::stdout().is_tty()
}

/// The local terminal size, or the size assumed without one.
pub(crate) fn size() -> TerminalSize {
    terminal_size().map_or(FALLBACK_SIZE, TerminalSize::from)
}

/// Starts watching the local terminal: its bytes, its size, and the signals
/// that mean the user wants to stop.
pub(crate) fn events() -> mpsc::Receiver<Event> {
    let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
    read_input(sender.clone());
    watch(SignalKind::window_change(), sender.clone(), || {
        Event::Resize(size())
    });
    for kind in [
        SignalKind::interrupt(),
        SignalKind::terminate(),
        SignalKind::hangup(),
    ] {
        watch(kind, sender.clone(), || Event::Interrupt);
    }
    receiver
}

/// Reads the terminal on a thread of its own.
///
/// A blocking read of standard input cannot be cancelled, so reading it on
/// tokio's blocking pool would keep the runtime from shutting down at the end
/// of the process; a plain thread is simply abandoned when the process exits.
fn read_input(sender: mpsc::Sender<Event>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut buffer = vec![0; INPUT_CHUNK];
        loop {
            match stdin.lock().read(&mut buffer) {
                Ok(0) | Err(_) => {
                    let _ = sender.blocking_send(Event::Eof);
                    return;
                }
                Ok(read) => {
                    if sender
                        .blocking_send(Event::Input(buffer[..read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
}

/// Turns one signal into a stream of events.
fn watch(kind: SignalKind, sender: mpsc::Sender<Event>, event: fn() -> Event) {
    tokio::spawn(async move {
        let Ok(mut signals) = signal(kind) else {
            return;
        };
        while signals.recv().await.is_some() {
            if sender.send(event()).await.is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossterm_dimensions_are_column_first() {
        assert_eq!(
            TerminalSize::from((123, 37)),
            TerminalSize {
                rows: 37,
                cols: 123
            }
        );
    }
}
