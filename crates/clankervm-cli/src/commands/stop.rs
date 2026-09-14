use crate::client::{MicroVmClient, MicroVmClientError};
use crate::output::render;
use crate::{ClankerError, OutputFormat};
use aws_sdk_lambdamicrovms::types::MicrovmState;
use clap::Args;
use serde::Serialize;
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Args)]
pub struct StopOptions {
    /// MicroVM to terminate.
    pub microvm_id: String,
    /// Wait until AWS confirms termination.
    #[arg(long)]
    pub wait: bool,
    /// Bound the entire stop operation.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "30s")]
    pub timeout: Duration,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StopResult {
    microvm_id: String,
    termination_confirmed: bool,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &StopOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    if options.timeout.is_zero() {
        return Err(ClankerError::InvalidConfig(
            "stop timeout must be positive".into(),
        ));
    }
    let deadline = Instant::now() + options.timeout;
    let details = bounded(deadline, client.describe(&options.microvm_id)).await?;
    let mut confirmed = details.is_none()
        || details
            .as_ref()
            .is_some_and(|details| details.state == MicrovmState::Terminated);
    let terminating = details
        .as_ref()
        .is_some_and(|details| details.state == MicrovmState::Terminating);
    if !confirmed && !terminating {
        match bounded(deadline, client.terminate(&options.microvm_id)).await {
            Ok(()) => {}
            Err(error @ ClankerError::MicroVmClient(MicroVmClientError::Conflict { .. })) => {
                let details = bounded(deadline, client.describe(&options.microvm_id)).await?;
                confirmed = details
                    .as_ref()
                    .is_none_or(|details| details.state == MicrovmState::Terminated);
                let terminating = details
                    .as_ref()
                    .is_some_and(|details| details.state == MicrovmState::Terminating);
                if !confirmed && !terminating {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    if options.wait && !confirmed {
        confirmed = confirm_termination(client, &options.microvm_id, deadline).await?;
    }
    let result = StopResult {
        microvm_id: options.microvm_id.clone(),
        termination_confirmed: confirmed,
    };
    render(format, &result, || {
        if confirmed {
            format!("✓ MicroVM {} is terminated", result.microvm_id)
        } else {
            format!("✓ Termination requested for MicroVM {}", result.microvm_id)
        }
    })
}

pub(crate) async fn terminate_and_confirm<T: MicroVmClient>(
    client: &T,
    microvm_id: &str,
    duration: Duration,
) -> Result<(), ClankerError> {
    let deadline = Instant::now() + duration;
    match bounded(deadline, client.terminate(microvm_id)).await {
        Ok(()) | Err(ClankerError::MicroVmClient(MicroVmClientError::Conflict { .. })) => {}
        Err(error) => return Err(error),
    }
    if confirm_termination(client, microvm_id, deadline).await? {
        Ok(())
    } else {
        Err(ClankerError::MicroVmTerminationUnconfirmed(
            microvm_id.into(),
        ))
    }
}

async fn confirm_termination<T: MicroVmClient>(
    client: &T,
    microvm_id: &str,
    deadline: Instant,
) -> Result<bool, ClankerError> {
    loop {
        let details = match bounded(deadline, client.describe(microvm_id)).await {
            Ok(details) => details,
            Err(ClankerError::OperationTimeout) => return Ok(false),
            Err(error) => return Err(error),
        };
        if details.is_none_or(|details| details.state == MicrovmState::Terminated) {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

async fn bounded<T>(
    deadline: Instant,
    future: impl Future<Output = Result<T, crate::client::MicroVmClientError>>,
) -> Result<T, ClankerError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ClankerError::OperationTimeout);
    }
    timeout(remaining, future)
        .await
        .map_err(|_| ClankerError::OperationTimeout)?
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient};
    use crate::test_support::MicroVmDetailsBuilder;

    #[tokio::test(start_paused = true)]
    async fn cleanup_recovers_from_a_termination_conflict_race() {
        let client = FakeMicroVmClient::default()
            .terminated([Err(MicroVmClientError::Conflict {
                operation: "terminate MicroVM",
                message: "already stopping".into(),
            })])
            .described([
                Ok(Some(
                    MicroVmDetailsBuilder::new("vm-1")
                        .state(MicrovmState::Terminating)
                        .build(),
                )),
                Ok(None),
            ]);

        terminate_and_confirm(&client, "vm-1", Duration::from_secs(10))
            .await
            .unwrap();

        assert_eq!(
            client.calls(),
            [
                Call::Terminate("vm-1".into()),
                Call::Describe("vm-1".into()),
                Call::Describe("vm-1".into()),
            ]
        );
    }
}
