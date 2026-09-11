mod arn;
mod artifact;
mod client;
mod commands;
mod config;
mod output;
mod payload;
mod release;
mod shell;
#[cfg(test)]
mod test_support;
mod util;

use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

pub use client::MicroVmClientError;
pub use commands::{Command, execute};
pub use shell::ShellError;

#[derive(Debug, Parser)]
#[command(
    name = "clankervm",
    about = "Bundle, push, and run AWS Lambda MicroVM apps"
)]
pub struct Cli {
    #[arg(long, global = true, default_value = "clankervm.toml")]
    pub config: PathBuf,
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,
    #[arg(long, global = true)]
    pub region: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Error)]
pub enum ClankerError {
    #[error(
        "failed to read project config {path}: {source}\n\nCreate one with: clankervm init --name <app>"
    )]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid project config {path}: {source}")]
    Config {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to {action}: {source}")]
    Io {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error("image {0} does not exist; push a release first")]
    ImageNotFound(String),
    #[error(transparent)]
    MicroVmClient(#[from] MicroVmClientError),
    #[error("release {release} failed: {reason}\nBuild logs: {log_group}")]
    ReleaseFailed {
        release: String,
        reason: String,
        log_group: String,
    },
    #[error(
        "timed out after {timeout:?} waiting for {release}\nResume with: clankervm status --wait {release}"
    )]
    WaitTimeout { release: String, timeout: Duration },
    #[error("timed out after {timeout:?} listing MicroVMs; raise --timeout or narrow the filters")]
    ListTimeout { timeout: Duration },
    #[error(
        "timed out after {timeout:?} reading {stream} in {group}; raise --timeout or narrow --since"
    )]
    LogsTimeout {
        timeout: Duration,
        group: String,
        stream: String,
    },
    #[error(
        "no log stream `{stream}` in log group `{group}`{}; pass --log-group or --log-stream to read another destination",
        streams_hint(streams)
    )]
    LogStreamNotFound {
        group: String,
        stream: String,
        streams: Vec<String>,
    },
    #[error("project file already exists: {0}; pass --force to replace it")]
    AlreadyInitialized(PathBuf),
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Shell(#[from] shell::ShellError),
    #[error(
        "`clankervm shell` needs an interactive terminal; run it from a terminal, not from a pipe or a script"
    )]
    NotATerminal,
    #[error("MicroVM `{0}` does not exist")]
    MicroVmNotFound(String),
    #[error(
        "MicroVM `{microvm_id}` is {state}, not RUNNING{}",
        reason.as_deref().map_or(String::new(), |reason| format!(" ({reason})"))
    )]
    MicroVmNotRunning {
        microvm_id: String,
        state: String,
        reason: Option<String>,
    },
    #[error("timed out after {timeout:?} waiting for MicroVM `{microvm_id}`")]
    MicroVmWaitTimeout {
        microvm_id: String,
        timeout: Duration,
    },
    #[error("MicroVM `{0}` was not confirmed terminated; check it with `clankervm list`")]
    MicroVmTerminationUnconfirmed(String),
}

/// How many stream names an error names before it stops listing them.
const MAX_LISTED_STREAMS: usize = 5;

/// The streams a log group does have, so a wrong stream name is obvious.
fn streams_hint(streams: &[String]) -> String {
    if streams.is_empty() {
        return String::new();
    }
    let listed: Vec<&str> = streams
        .iter()
        .take(MAX_LISTED_STREAMS)
        .map(String::as_str)
        .collect();
    format!("; the group has: {}", listed.join(", "))
}
