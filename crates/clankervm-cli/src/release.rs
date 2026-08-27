use crate::client::{ImageReleasePhase, InspectImageRequest, MicroVmClient, ObservedImageRelease};
use crate::config::{ProjectConfig, SelectedImage};
use crate::util::parse_release;
use crate::{Arn, ClankerError};
use futures_util::StreamExt;
use serde::Serialize;
use std::time::Duration;
use tokio::time::{Instant, sleep_until};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) struct Release {
    app: String,
    identifier: String,
    image_arn: Arn,
    image_version: String,
    bundle_digest: Option<String>,
    artifact_uri: Option<String>,
    build_log_group: String,
}

impl Release {
    pub(crate) fn new(
        app: &str,
        image_arn: Arn,
        image_version: &str,
        bundle_digest: Option<String>,
        artifact_uri: Option<String>,
    ) -> Self {
        Self {
            app: app.into(),
            identifier: format!("{app}@{image_version}"),
            image_arn,
            image_version: image_version.into(),
            bundle_digest,
            artifact_uri,
            build_log_group: format!("/aws/lambda-microvms/{app}"),
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
                    observed.image_state.as_str().into(),
                    observed.version_state.as_str().into(),
                    observed.version_status.as_str().into(),
                    observed.state_reason.clone(),
                )
            },
        );
        ReleaseStatus {
            app: self.app.clone(),
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
    pub app: String,
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

pub(crate) struct ResolvedImage {
    pub name: String,
    pub region: String,
    pub push: crate::commands::PushConfig,
    pub run: crate::commands::RunConfig,
    pub version: Option<String>,
}

pub(crate) fn resolve_image(
    config: &ProjectConfig,
    image: Option<&str>,
    release: Option<&str>,
) -> Result<ResolvedImage, ClankerError> {
    let parsed = release.map(parse_release).transpose()?;
    let release_name = parsed.map(|(name, _)| name);
    let version = parsed.map(|(_, version)| version.to_owned());
    let SelectedImage {
        name,
        region,
        push,
        run,
    } = config.select_image(image, release_name)?;
    Ok(ResolvedImage {
        name,
        region,
        push,
        run,
        version,
    })
}

pub(crate) struct ReleaseTarget {
    pub name: String,
    pub version: Option<String>,
    pub image_arn: Arn,
}

pub(crate) fn release_target(
    image: &ResolvedImage,
    account_role: &str,
) -> Result<ReleaseTarget, ClankerError> {
    let account = account_from_role(account_role)?;
    Ok(ReleaseTarget {
        name: image.name.clone(),
        version: image.version.clone(),
        image_arn: Arn::image(&image.name, &image.region, account)?,
    })
}

pub(crate) fn configured_account_role(image: &ResolvedImage) -> Result<&str, ClankerError> {
    image
        .push
        .build_role_arn
        .as_deref()
        .or(image.run.execution_role_arn.as_deref())
        .ok_or_else(|| {
            ClankerError::InvalidConfig(
                "push.build-role-arn or run.execution-role-arn must be configured".into(),
            )
        })
}

fn account_from_role(role: &str) -> Result<&str, ClankerError> {
    role.split(':')
        .nth(4)
        .filter(|account| !account.is_empty())
        .ok_or_else(|| ClankerError::InvalidConfig(format!("invalid IAM role ARN `{role}`")))
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
    let mut updates =
        Box::pin(client.poll_image_release(release.inspect_request(), observed, POLL_INTERVAL));
    let deadline = Instant::now() + timeout;
    let mut first = true;
    loop {
        let update = if first {
            first = false;
            updates.next().await
        } else {
            tokio::select! {
                update = updates.next() => update,
                () = sleep_until(deadline) => return Err(wait_timeout(&release, timeout)),
            }
        };
        let observed =
            update.expect("image release stream only ends after a terminal observation")?;
        let Some(observed) = observed else {
            if Instant::now() >= deadline {
                return Err(wait_timeout(&release, timeout));
            }
            continue;
        };
        let phase = observed.phase();
        let status = release.status(Some(&observed));
        report(&status);
        match phase {
            ImageReleasePhase::Pending => {}
            ImageReleasePhase::Ready => return Ok(status),
            ImageReleasePhase::Failed => {
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
