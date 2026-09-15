mod connect;
mod describe;
mod init;
mod list;
mod logs;
mod push;
mod run;
mod shell;
mod status;
mod stop;

use crate::application::{failure, select};
use crate::client::AwsMicroVmClient;
use crate::config::ProjectConfig;
use crate::{ClankerError, Cli, OutputFormat};
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_sdk_lambdamicrovms::config::ProvideCredentials;
use clap::Subcommand;
pub use connect::ConnectOptions;
pub use describe::CommandOptions as DescribeOptions;
use init::InitArgs;
pub use list::ListOptions;
pub use logs::LogsOptions;
pub(crate) use logs::LogsSettings;
pub use push::PushOptions;
pub(crate) use push::PushSettings;
pub use run::RunOptions;
pub(crate) use run::RunSettings;
pub use shell::ShellOptions;
pub use status::StatusOptions;
pub(crate) use status::StatusSettings;
use std::time::Duration;
pub use stop::CommandOptions as StopOptions;
use tokio::time::timeout;

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
    /// Connect to a named application without launching or stopping a VM.
    Connect(ConnectOptions),
    /// Describe a MicroVM without requesting an application token.
    Describe(DescribeOptions),
    /// Stop a MicroVM, optionally waiting for confirmation.
    Stop(StopOptions),
    /// Read the logs of a MicroVM.
    Logs(LogsOptions),
    /// Attach a terminal to a MicroVM's pty, launching one if needed.
    Shell(ShellOptions),
}

async fn create_sdk_config(config: &ProjectConfig) -> Result<SdkConfig, ClankerError> {
    timeout(Duration::from_secs(30), async {
        let mut loader = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(config.aws.region.clone()));
        if let Some(profile) = &config.aws.profile {
            loader = loader.profile_name(profile);
        }
        let sdk = loader.load().await;
        sdk.credentials_provider()
            .ok_or_else(|| failure("AWS credentials provider is unavailable"))?
            .provide_credentials()
            .await
            .map_err(|_| failure("AWS credential setup failed"))?;
        if let Some(expected) = &config.aws.expected_account_id {
            let identity = aws_sdk_sts::Client::new(&sdk)
                .get_caller_identity()
                .send()
                .await
                .map_err(|_| failure("AWS account identity check failed"))?;
            if identity.account() != Some(expected.as_str()) {
                return Err(failure(
                    "AWS account does not match aws.expected-account-id",
                ));
            }
        }
        Ok(sdk)
    })
    .await
    .map_err(|_| failure("AWS credential/account setup timed out"))?
}

fn preflight(
    command: &Command,
    config: &ProjectConfig,
    format: OutputFormat,
) -> Result<(), ClankerError> {
    match command {
        Command::Run(options) => options.connection(config, format).map(drop),
        Command::Connect(options) => {
            select(config, &options.name, options.connect_timeout, format).map(drop)
        }
        _ => Ok(()),
    }
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
    preflight(&cli.command, &config, cli.format)?;
    let sdk = create_sdk_config(&config).await?;
    let client = AwsMicroVmClient::new(&sdk);

    match cli.command {
        Command::Push(options) => push::execute(&options, &config, cli.format, &client).await,
        Command::Status(options) => status::execute(&options, &config, cli.format, &client).await,
        Command::List(options) => list::execute(&options, &config, cli.format, &client).await,
        Command::Run(options) => run::run(options, &config, cli.format, &client).await,
        Command::Connect(options) => connect::connect(options, &config, cli.format, &client).await,
        Command::Describe(options) => describe::describe(options, cli.format, &client).await,
        Command::Stop(options) => stop::stop(options, cli.format, &client).await,
        Command::Logs(options) => logs::execute(&options, &config, cli.format, &client).await,
        Command::Shell(options) => Box::pin(shell::execute(&options, &config, &client)).await,
        Command::Init(_) => unreachable!("init returns before project setup"),
    }
}
