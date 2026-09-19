#[cfg(test)]
#[path = "readiness_tests.rs"]
mod tests;

use crate::ClankerError;
use crate::client::{MicroVmClient, MicroVmDetails, is_resuming};
use crate::util::POLL_INTERVAL;
use aws_sdk_lambdamicrovms::types::MicrovmState;
use tokio::time::sleep;

struct RunningWait<'a, T> {
    client: &'a T,
    microvm_id: &'a str,
    launched: bool,
    observed: bool,
    resume_requested: bool,
}

impl<'a, T: MicroVmClient> RunningWait<'a, T> {
    fn new(client: &'a T, microvm_id: &'a str, launched: bool) -> Self {
        Self {
            client,
            microvm_id,
            launched,
            observed: false,
            resume_requested: false,
        }
    }

    async fn run<U>(
        mut self,
        mut check: impl FnMut(MicroVmDetails) -> Result<Option<U>, ClankerError>,
    ) -> Result<U, ClankerError> {
        loop {
            let Some(details) = self.client.get_details(self.microvm_id).await? else {
                if !self.launched || self.observed {
                    return Err(ClankerError::MicroVmNotFound(self.microvm_id.to_owned()));
                }
                sleep(POLL_INTERVAL).await;
                continue;
            };
            self.observed = true;

            match &details.state {
                MicrovmState::Running => {
                    if let Some(found) = check(details)? {
                        return Ok(found);
                    }
                }
                MicrovmState::Pending | MicrovmState::Suspending => {}
                MicrovmState::Suspended => self.resume_once().await?,
                state if is_resuming(state) => {}
                state => return Err(self.unexpected_state(state, details.state_reason)),
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn resume_once(&mut self) -> Result<(), ClankerError> {
        if !self.resume_requested {
            self.client.resume(self.microvm_id).await?;
            self.resume_requested = true;
        }
        Ok(())
    }

    fn unexpected_state(&self, state: &MicrovmState, reason: Option<String>) -> ClankerError {
        ClankerError::UnexpectedMicroVmState {
            microvm_id: self.microvm_id.to_owned(),
            state: state.to_string(),
            reason,
            expected: "RUNNING",
        }
    }
}

pub(crate) async fn wait_until_running<T, U>(
    client: &T,
    microvm_id: &str,
    launched: bool,
    check: impl FnMut(MicroVmDetails) -> Result<Option<U>, ClankerError>,
) -> Result<U, ClankerError>
where
    T: MicroVmClient,
{
    RunningWait::new(client, microvm_id, launched)
        .run(check)
        .await
}
