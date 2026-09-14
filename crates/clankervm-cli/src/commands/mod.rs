mod connect;
mod init;
mod inspect;
mod list;
mod logs;
mod push;
mod run;
mod shell;
mod status;
mod stop;

use crate::client::AwsMicroVmClient;
use crate::config::ProjectConfig;
use crate::{ClankerError, Cli};
use aws_config::{BehaviorVersion, Region};
use clap::Subcommand;
pub use connect::ConnectOptions;
use init::InitArgs;
pub use inspect::InspectOptions;
pub use list::ListOptions;
pub use logs::LogsOptions;
pub(crate) use logs::LogsSettings;
pub use push::PushOptions;
pub(crate) use push::PushSettings;
pub(crate) use run::RunSettings;
#[allow(unused_imports)]
pub use run::{RunCommandOptions, RunOptions};
pub use shell::ShellOptions;
pub use status::StatusOptions;
pub(crate) use status::StatusSettings;
pub use stop::StopOptions;

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
    Run(RunCommandOptions),
    /// Connect to a named application on an existing MicroVM.
    Connect(ConnectOptions),
    /// Inspect one MicroVM.
    Inspect(InspectOptions),
    /// Stop one MicroVM.
    Stop(StopOptions),
    /// Read the logs of a MicroVM.
    Logs(LogsOptions),
    /// Attach a terminal to a MicroVM's pty, launching one if needed.
    Shell(ShellOptions),
}

async fn validate_account(
    config: &ProjectConfig,
    sdk: &aws_config::SdkConfig,
) -> Result<(), ClankerError> {
    let Some(expected) = &config.aws.expected_account_id else {
        return Ok(());
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        aws_sdk_sts::Client::new(sdk).get_caller_identity().send(),
    )
    .await
    .map_err(|_| ClankerError::AccountValidation("timed out".into()))?
    .map_err(|error| ClankerError::AccountValidation(error.to_string()))?;
    let actual = output.account().unwrap_or_default();
    if actual != expected {
        return Err(ClankerError::AccountMismatch {
            expected: expected.clone(),
            actual: actual.into(),
        });
    }
    Ok(())
}

pub async fn execute(cli: Cli) -> Result<(), ClankerError> {
    if let Command::Init(args) = &cli.command {
        return init::execute(args, &cli.config, cli.format);
    }

    if let Command::Shell(_) = &cli.command {
        // `shell` needs a terminal before it needs a project or credentials.
        shell::preflight(cli.format)?;
    }

    let config = ProjectConfig::load(&cli.config, cli.region)?;
    match &cli.command {
        Command::Connect(options) => {
            let _ = connect::preflight(
                &options.name,
                options.connect_timeout,
                &options.arguments,
                &config,
                cli.format,
            )?;
        }
        Command::Run(options) => run::preflight(options, &config, cli.format)?,
        _ => {}
    }
    let mut sdk_loader = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(config.aws.region.clone()));

    if let Some(profile) = &config.aws.profile {
        sdk_loader = sdk_loader.profile_name(profile);
    }

    let sdk = sdk_loader.load().await;
    validate_account(&config, &sdk).await?;
    let client = AwsMicroVmClient::new(&sdk);

    match &cli.command {
        Command::Push(options) => push::execute(options, &config, cli.format, &client).await,
        Command::Status(options) => status::execute(options, &config, cli.format, &client).await,
        Command::List(options) => list::execute(options, &config, cli.format, &client).await,
        Command::Run(options) => {
            Box::pin(run::execute(
                options,
                &config,
                &cli.config,
                cli.format,
                &client,
            ))
            .await
        }
        Command::Connect(options) => {
            let selected = connect::preflight(
                &options.name,
                options.connect_timeout,
                &options.arguments,
                &config,
                cli.format,
            )?;
            connect::execute(options, selected, &config, &cli.config, cli.format, &client).await
        }
        Command::Inspect(options) => inspect::execute(options, cli.format, &client).await,
        Command::Stop(options) => Box::pin(stop::execute(options, cli.format, &client)).await,
        Command::Logs(options) => logs::execute(options, &config, cli.format, &client).await,
        Command::Shell(options) => Box::pin(shell::execute(options, &config, &client)).await,
        Command::Init(_) => unreachable!("init returns before project setup"),
    }
}
