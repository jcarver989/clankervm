//! Attaching the local terminal to the pty of a MicroVM launched with the
//! `SHELL_INGRESS` connector.

mod session;
mod socket;
mod terminal;

use std::io;
use thiserror::Error;

pub(crate) use session::{Options, attach};
pub(crate) use socket::{connect, request};
pub(crate) use terminal::{Event, events, is_interactive};

#[cfg(test)]
pub(crate) use socket::scripted::{Remote, scripted};

/// What went wrong while attaching a terminal to a MicroVM.
#[derive(Debug, Error)]
pub enum ShellError {
    #[error("invalid MicroVM endpoint `{0}`; expected a host or an https:// URL")]
    InvalidEndpoint(String),
    #[error("failed to connect to the MicroVM shell: {0}")]
    Connect(String),
    #[error(
        "the MicroVM rejected the shell token: it has expired, or the MicroVM was \
         not launched with the SHELL_INGRESS ingress connector"
    )]
    Unauthorized,
    #[error("the MicroVM's shell endpoint did not send its session preamble")]
    Handshake,
    #[error("timed out {0}")]
    Timeout(&'static str),
    #[error("the session was interrupted")]
    Interrupted,
    #[error("terminal error: {0}")]
    Terminal(#[from] io::Error),
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// The user left the session with Ctrl-] instead of the shell exiting.
    pub detached: bool,
}
