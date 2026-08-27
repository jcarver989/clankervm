mod arn;
mod bundle;
mod client;
mod commands;
mod config;
mod output;
mod payload;
mod release;
mod tags;
#[cfg(test)]
mod test_support;
mod util;

use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

pub use client::MicroVmClientError;
pub use commands::Command;
pub use payload::{PayloadError, build_run_payload};

pub(crate) use arn::Arn;
pub(crate) use client::AwsMicroVmClient;
pub(crate) use config::Project;
pub(crate) use tags::Tags;

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
    #[error("invalid release `{0}`; expected NAME@VERSION")]
    InvalidRelease(String),
    #[error("invalid image `{0}`")]
    InvalidImage(String),
    #[error("invalid ARN `{0}`")]
    InvalidArn(String),
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
    #[error("project file already exists: {0}; pass --force to replace it")]
    AlreadyInitialized(PathBuf),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
}

pub async fn execute(cli: Cli) -> Result<(), ClankerError> {
    commands::execute(cli.command, cli.config, cli.format, cli.region).await
}
