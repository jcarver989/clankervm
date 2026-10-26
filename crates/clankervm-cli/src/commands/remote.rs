use super::list::collect;
use super::run::{LaunchOptions, launch_vm};
use crate::ClankerError;
use crate::client::{MicroVmClient, MicroVmSummary};
use crate::config::ProjectConfig;
use aws_sdk_lambdamicrovms::types::MicrovmState;
use aws_smithy_types::DateTime;
use clap::{Args, Subcommand};
use serde::Serialize;
use std::time::Duration;
use tokio::time::timeout;

const LIST_TIMEOUT: Duration = Duration::from_mins(1);

/// What apps that open terminals and agents on remotes run to list and launch this project's
/// MicroVMs. Everything else they do runs the other commands as they are.
#[derive(Debug, Args)]
pub struct RemoteOptions {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Print the project's MicroVMs that have not been terminated, as
    /// `{"instances": [{"id", "state", "expires_at"}]}`.
    List,
    /// Launch a MicroVM as `run` would, and print its id.
    Launch(Box<LaunchOptions>),
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Instances {
    pub instances: Vec<Instance>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Instance {
    pub id: String,
    pub state: &'static str,
    /// When the MicroVM reaches the project's `max-duration`, in RFC 3339.
    pub expires_at: String,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: RemoteOptions,
    config: &ProjectConfig,
    client: &T,
) -> Result<(), ClankerError> {
    match options.command {
        RemoteCommand::List => {
            println!(
                "{}",
                serde_json::to_string(&instances(config, client).await?)?
            );
        }
        RemoteCommand::Launch(launch) => {
            println!("{}", launch_vm(&launch, config, client).await?.microvm_id);
        }
    }
    Ok(())
}

/// The MicroVMs of the project's image. AWS finds nothing for a bare image name, so this lists
/// the account's MicroVMs and keeps those whose image ARN names the project's image.
pub(crate) async fn instances<T: MicroVmClient>(
    config: &ProjectConfig,
    client: &T,
) -> Result<Instances, ClankerError> {
    let suffix = format!(":microvm-image:{}", config.name);
    let max_duration = i64::from(config.run.max_duration());
    let collected = timeout(LIST_TIMEOUT, collect(client, None, None))
        .await
        .map_err(|_| ClankerError::ListTimeout {
            timeout: LIST_TIMEOUT,
        })??;
    let instances = collected
        .into_iter()
        .filter(|microvm| microvm.image_arn.ends_with(&suffix))
        .filter_map(|microvm| instance(microvm, max_duration))
        .collect();
    Ok(Instances { instances })
}

fn instance(microvm: MicroVmSummary, max_duration: i64) -> Option<Instance> {
    let state = match microvm.state {
        MicrovmState::Running => "running",
        MicrovmState::Suspending => "suspending",
        MicrovmState::Suspended => "suspended",
        MicrovmState::Terminating => "stopping",
        MicrovmState::Terminated => return None,
        _ => "starting",
    };
    let expires_at = DateTime::from_secs(microvm.started_at.secs() + max_duration).to_string();
    Some(Instance {
        id: microvm.microvm_id,
        state,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::FakeMicroVmClient;
    use crate::test_support::{IMAGE_ARN, MicroVmSummaryBuilder, ROLE, project};
    use tempfile::TempDir;

    #[tokio::test]
    async fn the_projects_microvms_are_listed_with_when_they_expire() {
        let directory = TempDir::new().unwrap();
        let config = project(
            directory.path(),
            &format!("[microvm.run]\n{ROLE}max-duration = 7200\n"),
        );
        let client = FakeMicroVmClient::default().listed([Ok((
            vec![
                MicroVmSummaryBuilder::new("microvm-run")
                    .started_at(1_787_616_000)
                    .build(),
                MicroVmSummaryBuilder::new("microvm-new")
                    .state(MicrovmState::Pending)
                    .build(),
                MicroVmSummaryBuilder::new("microvm-waking")
                    .state(MicrovmState::from("RESUMING"))
                    .build(),
                MicroVmSummaryBuilder::new("microvm-ending")
                    .state(MicrovmState::Terminating)
                    .build(),
                MicroVmSummaryBuilder::new("microvm-gone")
                    .state(MicrovmState::Terminated)
                    .build(),
                MicroVmSummaryBuilder::new("microvm-other")
                    .image_arn(&IMAGE_ARN.replace(":demo", ":other"))
                    .build(),
            ],
            None,
        ))]);

        let listed = instances(&config, &client).await.unwrap();

        let states: Vec<(&str, &str)> = listed
            .instances
            .iter()
            .map(|instance| (instance.id.as_str(), instance.state))
            .collect();
        assert_eq!(
            states,
            [
                ("microvm-run", "running"),
                ("microvm-new", "starting"),
                ("microvm-waking", "starting"),
                ("microvm-ending", "stopping"),
            ]
        );
        assert_eq!(listed.instances[0].expires_at, "2026-08-25T02:00:00Z");
    }

    #[tokio::test]
    async fn the_listing_is_the_remotes_contract() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), "");
        let client = FakeMicroVmClient::default().listed([Ok((
            vec![MicroVmSummaryBuilder::new("microvm-1").build()],
            None,
        ))]);

        let listed = instances(&config, &client).await.unwrap();

        assert_eq!(
            serde_json::to_value(&listed).unwrap(),
            serde_json::json!({"instances": [
                {"id": "microvm-1", "state": "running", "expires_at": "1970-01-01T01:00:00Z"},
            ]})
        );
    }
}
