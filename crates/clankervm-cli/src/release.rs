use crate::ClankerError;
use crate::arn::Arn;
use crate::client::{MicroVmClient, Observation};
use serde::Serialize;
use std::time::Duration;
use tokio::time::{Instant, sleep, sleep_until};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// One release: an image version pinned by name.
#[derive(Clone, Debug)]
pub(crate) struct Release {
    name: String,
    pub(crate) arn: Arn,
    pub(crate) version: String,
    bundle_digest: Option<String>,
    artifact_uri: Option<String>,
}

impl Release {
    pub(crate) fn new(name: &str, arn: Arn, version: &str) -> Self {
        Self {
            name: name.into(),
            arn,
            version: version.into(),
            bundle_digest: None,
            artifact_uri: None,
        }
    }

    /// Records the artifact a `push` published.
    pub(crate) fn with_artifact(mut self, digest: String, artifact_uri: String) -> Self {
        self.bundle_digest = Some(digest);
        self.artifact_uri = Some(artifact_uri);
        self
    }

    pub(crate) fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    pub(crate) fn status(&self, observed: Option<&Observation>) -> ReleaseStatus {
        ReleaseStatus {
            image_name: self.name.clone(),
            release: self.id(),
            image_arn: self.arn.clone(),
            observation: Observation {
                image_version: self.version.clone(),
                ..observed.cloned().unwrap_or_default()
            },
            bundle_digest: self.bundle_digest.clone(),
            artifact_uri: self.artifact_uri.clone(),
            build_log_group: format!("/aws/lambda-microvms/{}", self.name),
        }
    }
}

/// What a release looks like right now, including local artifact details.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReleaseStatus {
    pub image_name: String,
    pub release: String,
    pub image_arn: Arn,
    #[serde(flatten)]
    pub observation: Observation,
    pub bundle_digest: Option<String>,
    pub artifact_uri: Option<String>,
    pub build_log_group: String,
}

pub(crate) async fn wait_for_release<T, F>(
    client: &T,
    release: Release,
    observed: Option<Observation>,
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
                result = client.observe(&release.arn, Some(&release.version)) => result?,
                () = sleep_until(deadline) => return Err(wait_timeout(&release, timeout)),
            }
        };

        if let Some(current) = current {
            let status = release.status(Some(&current));
            report(&status);
            if current.is_ready() {
                return Ok(status);
            }
            if !current.is_pending() {
                return Err(ClankerError::ReleaseFailed {
                    release: status.release,
                    reason: current
                        .state_reason
                        .unwrap_or_else(|| "AWS did not provide a failure reason".into()),
                    log_group: status.build_log_group,
                });
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
        release: release.id(),
        timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FakeMicroVmClient, MicroVmClientError};
    use crate::test_support::{active, failed, pending};

    fn release() -> Release {
        Release::new("demo", arn(), "2")
    }

    #[tokio::test(start_paused = true)]
    async fn wait_polls_until_the_release_is_active() {
        let client =
            FakeMicroVmClient::default().observed([Ok(Some(pending("2"))), Ok(Some(active("2")))]);

        let result = wait_for_release(&client, release(), None, Duration::from_mins(1), |_| {})
            .await
            .unwrap();

        assert_eq!(result.observation.version_status, "ACTIVE");
        assert_eq!(client.calls().len(), 2);
    }

    #[tokio::test]
    async fn wait_surfaces_build_failures_with_their_reason() {
        let failed = Observation {
            state_reason: Some("build exploded".into()),
            ..failed("2")
        };
        let client = FakeMicroVmClient::default().observed([Ok(Some(failed))]);

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

    #[tokio::test]
    async fn wait_reports_client_failures() {
        let client = FakeMicroVmClient::default().observed([Err(MicroVmClientError::Service {
            operation: "get image",
            message: "boom".into(),
        })]);

        let error = wait_for_release(&client, release(), None, Duration::from_mins(1), |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
    }

    fn arn() -> Arn {
        Arn::parse("arn:aws:lambda:us-east-1:123456789012:microvm-image:demo").unwrap()
    }
}
