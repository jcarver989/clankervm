use crate::client::{InspectImageRequest, MicroVmClient, ObservedImageRelease, ReleasePhase};
use crate::{Arn, ClankerError};
use serde::Serialize;
use std::time::Duration;
use tokio::time::{Instant, sleep, sleep_until};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) struct Release {
    image_name: String,
    identifier: String,
    image_arn: Arn,
    image_version: String,
    bundle_digest: Option<String>,
    artifact_uri: Option<String>,
    build_log_group: String,
}

impl Release {
    pub(crate) fn new(
        image_name: &str,
        image_arn: Arn,
        image_version: &str,
        bundle_digest: Option<String>,
        artifact_uri: Option<String>,
    ) -> Self {
        Self {
            image_name: image_name.into(),
            identifier: format!("{image_name}@{image_version}"),
            image_arn,
            image_version: image_version.into(),
            bundle_digest,
            artifact_uri,
            build_log_group: format!("/aws/lambda-microvms/{image_name}"),
        }
    }

    pub(crate) fn inspect_request(&self) -> InspectImageRequest {
        InspectImageRequest {
            image_identifier: self.image_arn.clone(),
            image_version: Some(self.image_version.clone()),
        }
    }

    pub(crate) fn status(&self, observed: Option<&ObservedImageRelease>) -> ReleaseStatus {
        let (image_state, version_state, version_status, state_reason) = observed.map_or_else(
            || ("PENDING".into(), "PENDING".into(), "INACTIVE".into(), None),
            |observed| {
                (
                    observed.image_state.clone(),
                    observed.version_state.clone(),
                    observed.version_status.clone(),
                    observed.state_reason.clone(),
                )
            },
        );
        ReleaseStatus {
            image_name: self.image_name.clone(),
            release: self.identifier.clone(),
            image_arn: self.image_arn.clone(),
            image_version: self.image_version.clone(),
            image_state,
            version_state,
            version_status,
            state_reason,
            bundle_digest: self.bundle_digest.clone(),
            artifact_uri: self.artifact_uri.clone(),
            build_log_group: self.build_log_group.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReleaseStatus {
    pub image_name: String,
    pub release: String,
    pub image_arn: Arn,
    pub image_version: String,
    pub image_state: String,
    pub version_state: String,
    pub version_status: String,
    pub state_reason: Option<String>,
    pub bundle_digest: Option<String>,
    pub artifact_uri: Option<String>,
    pub build_log_group: String,
}

pub(crate) async fn wait_for_release<T, F>(
    client: &T,
    release: Release,
    observed: Option<ObservedImageRelease>,
    timeout: Duration,
    mut report: F,
) -> Result<ReleaseStatus, ClankerError>
where
    T: MicroVmClient,
    F: FnMut(&ReleaseStatus),
{
    let deadline = Instant::now() + timeout;
    let mut observed = observed;
    loop {
        let current = if let Some(observed) = observed.take() {
            Some(observed)
        } else {
            tokio::select! {
                result = client.inspect_image(release.inspect_request()) => result?,
                () = sleep_until(deadline) => return Err(wait_timeout(&release, timeout)),
            }
        };

        if let Some(current) = current {
            let phase = current.phase();
            let status = release.status(Some(&current));
            report(&status);
            match phase {
                ReleasePhase::Pending => {}
                ReleasePhase::Ready => return Ok(status),
                ReleasePhase::Failed => {
                    return Err(ClankerError::ReleaseFailed {
                        release: status.release,
                        reason: status
                            .state_reason
                            .unwrap_or_else(|| "AWS did not provide a failure reason".into()),
                        log_group: status.build_log_group,
                    });
                }
            }
        }

        tokio::select! {
            () = sleep(POLL_INTERVAL) => {}
            () = sleep_until(deadline) => return Err(wait_timeout(&release, timeout)),
        }
    }
}

fn wait_timeout(release: &Release, timeout: Duration) -> ClankerError {
    ClankerError::WaitTimeout {
        release: release.identifier.clone(),
        timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::FakeMicroVmClient;
    use crate::test_support::ObservedImageReleaseBuilder;

    fn release() -> Release {
        Release::new("demo", arn(), "2", None, None)
    }

    #[tokio::test(start_paused = true)]
    async fn wait_polls_until_the_release_is_active() {
        let client = FakeMicroVmClient::builder()
            .inspection_responses([
                Ok(Some(ObservedImageReleaseBuilder::pending("2"))),
                Ok(Some(ObservedImageReleaseBuilder::active("2"))),
            ])
            .build();

        let result = wait_for_release(&client, release(), None, Duration::from_mins(1), |_| {})
            .await
            .unwrap();

        assert_eq!(result.version_status, "ACTIVE");
        assert_eq!(client.calls().len(), 2);
    }

    #[tokio::test]
    async fn wait_surfaces_build_failures_with_their_reason() {
        let failed = ObservedImageReleaseBuilder::failed("2")
            .reason("build exploded")
            .build();
        let client = FakeMicroVmClient::builder()
            .inspection_responses([Ok(Some(failed))])
            .build();

        let error = wait_for_release(&client, release(), None, Duration::from_mins(1), |_| {})
            .await
            .unwrap_err();

        let ClankerError::ReleaseFailed {
            release, reason, ..
        } = error
        else {
            panic!("expected release failure, got {error}");
        };
        assert_eq!(release, "demo@2");
        assert_eq!(reason, "build exploded");
    }

    #[tokio::test]
    async fn wait_times_out_when_the_release_never_appears() {
        let client = FakeMicroVmClient::default();

        let error = wait_for_release(&client, release(), None, Duration::ZERO, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::WaitTimeout { .. }), "{error}");
        assert_eq!(client.calls().len(), 1);
    }

    fn arn() -> Arn {
        Arn::parse("arn:aws:lambda:us-east-1:123456789012:microvm-image:demo").unwrap()
    }
}
