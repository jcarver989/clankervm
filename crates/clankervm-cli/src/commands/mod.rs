mod init;
mod push;
mod run;
mod status;
use crate::{AwsMicroVmClient, ClankerError, OutputFormat, Project};
use aws_config::{BehaviorVersion, Region};
use clap::Subcommand;
use init::InitArgs;
use push::PushArgs;
pub(crate) use push::PushConfig;
use run::RunArgs;
pub(crate) use run::RunConfig;
use status::StatusArgs;
pub(crate) use status::StatusConfig;
use std::path::PathBuf;

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
    if let Command::Init(args) = &command {
        return init::execute(args, &config_path, format);
    }

    let project = Project::load(&config_path, region)?;
    let sdk = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(project.config.app.region.clone()))
        .load()
        .await;

    let client = AwsMicroVmClient::new(&sdk);
    match command {
        Command::Push(args) => push::execute(args, &project, format, &client).await,
        Command::Status(args) => status::execute(args, &project, format, &client).await,
        Command::Run(args) => run::execute(args, &project, format, &client).await,
        Command::Init(_) => unreachable!("init returns before project setup"),
    }
}
