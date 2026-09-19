#[cfg(test)]
#[path = "transition_tests.rs"]
mod tests;

use crate::ClankerError;
use crate::client::{AwsFailure, MicroVmClient, MicroVmClientError, MicroVmDetails, is_resuming};
use crate::util::POLL_INTERVAL;
use aws_sdk_lambdamicrovms::types::MicrovmState;
use std::time::Duration;
use tokio::time::{sleep, timeout};

#[derive(Clone, Copy, Debug)]
pub(super) enum Operation {
    Suspend,
    Resume,
    Stop,
}

struct Spec {
    action: &'static str,
    noun: &'static str,
    expected: &'static str,
    target: &'static str,
    confirmed_field: &'static str,
}

impl Operation {
    fn spec(self) -> Spec {
        match self {
            Self::Suspend => Spec {
                action: "suspend",
                noun: "suspension",
                expected: "RUNNING",
                target: "SUSPENDED",
                confirmed_field: "suspensionConfirmed",
            },
            Self::Resume => Spec {
                action: "resume",
                noun: "resumption",
                expected: "SUSPENDED",
                target: "RUNNING",
                confirmed_field: "resumptionConfirmed",
            },
            Self::Stop => Spec {
                action: "stop",
                noun: "termination",
                expected: "a live state",
                target: "TERMINATED",
                confirmed_field: "terminationConfirmed",
            },
        }
    }

    pub(super) fn noun(self) -> &'static str {
        self.spec().noun
    }

    pub(super) fn confirmed_field(self) -> &'static str {
        self.spec().confirmed_field
    }

    fn classify(self, details: MicroVmDetails) -> Progress {
        match self {
            Self::Suspend => match &details.state {
                MicrovmState::Suspended => Progress::Complete,
                MicrovmState::Suspending => Progress::InProgress,
                MicrovmState::Running => Progress::Requestable,
                _ => Progress::Invalid(details),
            },
            Self::Resume => match &details.state {
                MicrovmState::Running => Progress::Complete,
                MicrovmState::Suspended => Progress::Requestable,
                state if is_resuming(state) => Progress::InProgress,
                _ => Progress::Invalid(details),
            },
            Self::Stop => match &details.state {
                MicrovmState::Terminated => Progress::Complete,
                MicrovmState::Terminating => Progress::InProgress,
                _ => Progress::Requestable,
            },
        }
    }

    fn missing(self, microvm_id: &str) -> Result<Progress, ClankerError> {
        match self {
            Self::Stop => Ok(Progress::Complete),
            Self::Suspend | Self::Resume => {
                Err(ClankerError::MicroVmNotFound(microvm_id.to_owned()))
            }
        }
    }

    async fn request<T: MicroVmClient>(
        self,
        client: &T,
        microvm_id: &str,
    ) -> Result<(), MicroVmClientError> {
        match self {
            Self::Suspend => client.suspend(microvm_id).await,
            Self::Resume => client.resume(microvm_id).await,
            Self::Stop => client.terminate(microvm_id).await,
        }
    }

    fn conflict_error(self, microvm_id: &str, error: MicroVmClientError) -> ClankerError {
        match self {
            Self::Stop => ClankerError::MicroVmTerminationUnconfirmed(microvm_id.to_owned()),
            Self::Suspend | Self::Resume => error.into(),
        }
    }

    fn timeout_error(self, microvm_id: &str, duration: Duration) -> ClankerError {
        match self {
            Self::Stop => ClankerError::MicroVmTerminationUnconfirmed(microvm_id.to_owned()),
            Self::Suspend | Self::Resume => ClankerError::MicroVmLifecycleUnconfirmed {
                microvm_id: microvm_id.to_owned(),
                operation: self.spec().action,
                target: self.spec().target,
                timeout: duration,
            },
        }
    }

    fn invalid_state(self, microvm_id: &str, details: MicroVmDetails) -> ClankerError {
        ClankerError::UnexpectedMicroVmState {
            microvm_id: microvm_id.to_owned(),
            state: details.state.to_string(),
            reason: details.state_reason,
            expected: self.spec().expected,
        }
    }
}

#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
enum Progress {
    Complete,
    InProgress,
    Requestable,
    Invalid(MicroVmDetails),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Confirmation {
    Confirmed,
    InProgress,
}

impl Confirmation {
    pub(super) fn is_confirmed(self) -> bool {
        matches!(self, Self::Confirmed)
    }

    pub(super) fn human(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::InProgress => "in progress",
        }
    }
}

pub(super) struct LifecycleTransition<'a, T> {
    client: &'a T,
    microvm_id: &'a str,
    operation: Operation,
}

impl<'a, T: MicroVmClient> LifecycleTransition<'a, T> {
    pub(super) fn new(client: &'a T, microvm_id: &'a str, operation: Operation) -> Self {
        Self {
            client,
            microvm_id,
            operation,
        }
    }

    pub(super) async fn apply(
        &self,
        wait: bool,
        duration: Duration,
    ) -> Result<Confirmation, ClankerError> {
        timeout(duration, self.apply_before_timeout(wait))
            .await
            .unwrap_or_else(|_| Err(self.operation.timeout_error(self.microvm_id, duration)))
    }

    async fn apply_before_timeout(&self, wait: bool) -> Result<Confirmation, ClankerError> {
        match self.observe().await? {
            Progress::Complete => return Ok(Confirmation::Confirmed),
            Progress::InProgress => {}
            Progress::Requestable => self.request_or_reconcile_conflict().await?,
            Progress::Invalid(details) => return Err(self.invalid_state(details)),
        }

        if wait {
            self.wait_for_completion().await?;
            Ok(Confirmation::Confirmed)
        } else {
            Ok(Confirmation::InProgress)
        }
    }

    async fn observe(&self) -> Result<Progress, ClankerError> {
        match self.client.get_details(self.microvm_id).await? {
            Some(details) => Ok(self.operation.classify(details)),
            None => self.operation.missing(self.microvm_id),
        }
    }

    async fn request_or_reconcile_conflict(&self) -> Result<(), ClankerError> {
        match self.operation.request(self.client, self.microvm_id).await {
            Ok(()) => Ok(()),
            Err(
                error @ MicroVmClientError::AwsOperationFailed {
                    kind: AwsFailure::Conflict,
                    ..
                },
            ) => self.reconcile_conflict(error).await,
            Err(error) => Err(error.into()),
        }
    }

    async fn reconcile_conflict(&self, error: MicroVmClientError) -> Result<(), ClankerError> {
        match self.observe().await? {
            Progress::Complete | Progress::InProgress => Ok(()),
            Progress::Requestable | Progress::Invalid(_) => {
                Err(self.operation.conflict_error(self.microvm_id, error))
            }
        }
    }

    async fn wait_for_completion(&self) -> Result<(), ClankerError> {
        loop {
            sleep(POLL_INTERVAL).await;
            match self.observe().await? {
                Progress::Complete => return Ok(()),
                Progress::InProgress | Progress::Requestable => {}
                Progress::Invalid(details) => return Err(self.invalid_state(details)),
            }
        }
    }

    fn invalid_state(&self, details: MicroVmDetails) -> ClankerError {
        self.operation.invalid_state(self.microvm_id, details)
    }
}
