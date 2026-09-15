#[cfg(test)]
#[path = "stop_tests.rs"]
mod tests;

use crate::client::{AwsFailure, MicroVmClient, MicroVmClientError};
use crate::output::render;
use crate::util::positive_duration;
use crate::{ClankerError, OutputFormat};
use aws_sdk_lambdamicrovms::types::MicrovmState;
use clap::Args;
use serde::Serialize;
use std::time::Duration;
use tokio::time::{sleep, timeout};

/// How long AWS is given to confirm a termination it accepted.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Args)]
pub struct CommandOptions {
    pub vm_id: String,
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value = "30s", value_parser = positive_duration)]
    pub timeout: Duration,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StopResult {
    microvm_id: String,
    termination_confirmed: bool,
}

pub async fn stop<T: MicroVmClient>(
    options: CommandOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let confirmed = stop_vm(client, &options.vm_id, options.wait, options.timeout).await?;
    let result = StopResult {
        microvm_id: options.vm_id.clone(),
        termination_confirmed: confirmed,
    };
    render(format, &result, || {
        format!(
            "MicroVM {}: termination {}",
            result.microvm_id,
            if confirmed {
                "confirmed"
            } else {
                "in progress"
            }
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Progress {
    Gone,
    Terminating,
    Live,
}

async fn progress<T: MicroVmClient>(client: &T, id: &str) -> Result<Progress, ClankerError> {
    Ok(match client.get_details(id).await? {
        None => Progress::Gone,
        Some(vm) => match vm.state {
            MicrovmState::Terminated => Progress::Gone,
            MicrovmState::Terminating => Progress::Terminating,
            _ => Progress::Live,
        },
    })
}

pub(crate) async fn stop_vm<T: MicroVmClient>(
    client: &T,
    id: &str,
    wait: bool,
    duration: Duration,
) -> Result<bool, ClankerError> {
    timeout(duration, async {
        match progress(client, id).await? {
            Progress::Gone => return Ok(true),
            Progress::Terminating => {}
            Progress::Live => match client.terminate(id).await {
                Ok(()) => {}
                Err(MicroVmClientError::AwsOperationFailed {
                    kind: AwsFailure::Conflict,
                    ..
                }) => match progress(client, id).await? {
                    Progress::Gone => return Ok(true),
                    Progress::Terminating => {}
                    Progress::Live => {
                        return Err(ClankerError::MicroVmTerminationUnconfirmed(id.into()));
                    }
                },
                Err(error) => return Err(error.into()),
            },
        }
        if !wait {
            return Ok(false);
        }
        confirm(client, id).await?;
        Ok(true)
    })
    .await
    .unwrap_or_else(|_| Err(ClankerError::MicroVmTerminationUnconfirmed(id.into())))
}

async fn confirm<T: MicroVmClient>(client: &T, id: &str) -> Result<(), ClankerError> {
    loop {
        if progress(client, id).await? == Progress::Gone {
            return Ok(());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

/// Shared owned-VM cleanup, including the termination request in the deadline.
pub(crate) async fn terminate<T: MicroVmClient>(client: &T, id: &str) -> Result<(), ClankerError> {
    stop_vm(client, id, true, CLEANUP_TIMEOUT).await.map(drop)
}
