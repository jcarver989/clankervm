mod init;
mod list;
mod logs;
mod push;
mod run;
mod status;

use crate::client::AwsMicroVmClient;
use crate::config::ProjectConfig;
use crate::{ClankerError, Cli};
use aws_config::{BehaviorVersion, Region};
use clap::Subcommand;
use init::InitArgs;
pub use list::ListOptions;
pub use logs::LogsOptions;
pub(crate) use logs::LogsSettings;
pub use push::PushOptions;
pub(crate) use push::PushSettings;
pub use run::RunOptions;
pub(crate) use run::RunSettings;
pub use status::StatusOptions;
pub(crate) use status::StatusSettings;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new ClankerVM project file.
    Init(InitArgs),
    /// Release a prepared directory or ZIP as a new image version.
    Push(PushOptions),
    /// Inspect a release, optionally waiting for it to become active.
    Status(StatusOptions),
    /// List MicroVMs in the account.
    List(ListOptions),
    /// Start a command in a MicroVM.
    Run(RunOptions),
    /// Read the logs of a MicroVM.
    Logs(LogsOptions),
}

pub async fn execute(cli: Cli) -> Result<(), ClankerError> {
    if let Command::Init(args) = &cli.command {
        return init::execute(args, &cli.config, cli.format);
    }

    let config = ProjectConfig::load(&cli.config, cli.region)?;
    let mut sdk_loader = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(config.image.region.clone()));

    if let Some(profile) = &config.image.profile {
        sdk_loader = sdk_loader.profile_name(profile);
    }

    let sdk = sdk_loader.load().await;
    let client = AwsMicroVmClient::new(&sdk);

    match &cli.command {
        Command::Push(options) => push::execute(options, &config, cli.format, &client).await,
        Command::Status(options) => status::execute(options, &config, cli.format, &client).await,
        Command::List(options) => list::execute(options, &config, cli.format, &client).await,
        Command::Run(options) => run::execute(options, &config, cli.format, &client).await,
        Command::Logs(options) => logs::execute(options, &config, cli.format, &client).await,
        Command::Init(_) => unreachable!("init returns before project setup"),
    }
}
