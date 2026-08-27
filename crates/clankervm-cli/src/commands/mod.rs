mod init;
mod push;
mod run;
mod status;

use crate::config::ProjectConfig;
use crate::{AwsMicroVmClient, ClankerError, Project};
use clap::Subcommand;
use serde::Serialize;
use std::path::PathBuf;

pub use init::InitArgs;
pub use push::PushArgs;
pub use run::RunArgs;
pub use status::StatusArgs;

use crate::OutputFormat;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseStatus {
    pub app: String,
    pub release: String,
    pub image_arn: String,
    pub image_version: String,
    pub image_state: String,
    pub version_state: String,
    pub version_status: String,
    pub state_reason: Option<String>,
    pub bundle_digest: Option<String>,
    pub artifact_uri: Option<String>,
    pub build_log_group: String,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new ClankerVM project file.
    Init(InitArgs),
    /// Bundle the configured context and release a new image version.
    Push(PushArgs),
    /// Inspect a release, optionally waiting for it to become active.
    Status(StatusArgs),
    /// Start a command in a MicroVM.
    Run(RunArgs),
}

pub(crate) async fn execute(
    command: Command,
    config_path: PathBuf,
    format: OutputFormat,
    region: Option<String>,
) -> Result<(), ClankerError> {
    if let Command::Init(args) = command {
        return init::execute(&args, &config_path, format);
    }

    let project = Project::load(&config_path, region)?;
    match command {
        Command::Init(_) => unreachable!("init returned before loading the project"),
        Command::Push(args) => {
            let sdk = crate::sdk_for(project.config()).await;
            push::execute(args, &project, format, &AwsMicroVmClient::new(&sdk)).await
        }
        Command::Status(args) => {
            let sdk = crate::sdk_for(project.config()).await;
            status::execute(args, &project, format, &AwsMicroVmClient::new(&sdk)).await
        }
        Command::Run(args) => {
            let sdk = crate::sdk_for(project.config()).await;
            run::execute(args, &project, format, &AwsMicroVmClient::new(&sdk)).await
        }
    }
}

pub(super) fn image_account<'a>(
    config: &'a ProjectConfig,
    fallback_role: Option<&'a str>,
) -> Result<&'a str, ClankerError> {
    let role = config
        .push
        .build_role_arn
        .as_deref()
        .filter(|role| !role.is_empty())
        .or(fallback_role)
        .or(config.run.execution_role_arn.as_deref())
        .filter(|role| !role.is_empty())
        .ok_or_else(|| {
            ClankerError::InvalidConfig(
                "push.build-role-arn or run.execution-role-arn must be configured".into(),
            )
        })?;
    account_from_role(role)
}

fn account_from_role(role: &str) -> Result<&str, ClankerError> {
    role.split(':')
        .nth(4)
        .filter(|account| !account.is_empty())
        .ok_or_else(|| ClankerError::InvalidConfig(format!("invalid IAM role ARN `{role}`")))
}

pub(super) fn render<T: Serialize>(
    format: OutputFormat,
    value: &T,
    human: impl FnOnce() -> String,
) -> Result<(), ClankerError> {
    match format {
        OutputFormat::Human => println!("{}", human()),
        OutputFormat::Json => println!("{}", serde_json::to_string(value)?),
    }
    Ok(())
}
