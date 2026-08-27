use super::{ReleaseStatus, image_account, render};
use crate::client::{
    ImageState, ImageVersionState, ImageVersionStatus, InspectImageRequest, MicroVmClient,
    ObservedImageRelease,
};
use crate::{ClankerError, OutputFormat, Project, image_arn};
use clap::Args;
use std::time::{Duration, Instant};

pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub release: Option<String>,
    /// Wait until the release becomes active.
    #[arg(long)]
    pub wait: bool,
    #[arg(long)]
    pub timeout: Option<String>,
}

pub(super) async fn execute<T: MicroVmClient>(
    args: StatusArgs,
    project: &Project,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let (release, observed) = resolve_release(args.release.as_deref(), project, client).await?;
    let result = if args.wait {
        let timeout = duration_or(args.timeout.as_deref(), project.config().status.timeout)?;
        wait_for_release(client, release, observed, timeout).await?
    } else if let Some(observed) = observed {
        apply_observation(release, observed)
    } else {
        inspect_release(client, release).await?
    };
    render(format, &result, || status_text(&result))
}

async fn resolve_release<T: MicroVmClient>(
    requested: Option<&str>,
    project: &Project,
    client: &T,
) -> Result<(ReleaseStatus, Option<ObservedImageRelease>), ClankerError> {
    let config = project.config();
    if let Some(value) = requested {
        let (name, version) = value
            .rsplit_once('@')
            .ok_or_else(|| ClankerError::InvalidRelease(value.into()))?;
        let arn = image_arn(name, &config.app.region, Some(image_account(config, None)?))?;
        return Ok((pending_release(name, arn, version, None, None), None));
    }
    let arn = image_arn(
        &config.app.name,
        &config.app.region,
        Some(image_account(config, None)?),
    )?;
    let observed = client
        .inspect_image(InspectImageRequest {
            image_identifier: arn.clone(),
            image_version: None,
        })
        .await?
        .ok_or_else(|| ClankerError::InvalidImage(arn.clone()))?;
    let release = pending_release(&config.app.name, arn, &observed.image_version, None, None);
    Ok((release, Some(observed)))
}

pub(super) async fn wait_for_release<T: MicroVmClient>(
    client: &T,
    mut release: ReleaseStatus,
    mut observed: Option<ObservedImageRelease>,
    timeout: Duration,
) -> Result<ReleaseStatus, ClankerError> {
    let start = Instant::now();
    let mut prior = None;
    loop {
        let current = match observed.take() {
            Some(observed) => Some(observed),
            None => read_observation(client, &release).await?,
        };
        if let Some(current) = current {
            let state = format!(
                "{}:{}:{}",
                current.image_state.as_str(),
                current.version_state.as_str(),
                current.version_status.as_str()
            );
            if prior.as_deref() != Some(state.as_str()) {
                eprintln!(
                    "  Image: {:<10} Build: {:<10} Activation: {}",
                    current.image_state.as_str(),
                    current.version_state.as_str(),
                    current.version_status.as_str()
                );
                prior = Some(state);
            }
            if is_ready(&current) {
                return Ok(apply_observation(release, current));
            }
            if is_failed(&current) {
                return Err(ClankerError::ReleaseFailed {
                    release: release.release,
                    reason: current
                        .state_reason
                        .unwrap_or_else(|| "AWS did not provide a failure reason".into()),
                    log_group: release.build_log_group,
                });
            }
            release = apply_observation(release, current);
        }
        if start.elapsed() >= timeout {
            return Err(ClankerError::WaitTimeout {
                release: release.release,
                timeout,
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn inspect_release<T: MicroVmClient>(
    client: &T,
    release: ReleaseStatus,
) -> Result<ReleaseStatus, ClankerError> {
    Ok(match read_observation(client, &release).await? {
        Some(observed) => apply_observation(release, observed),
        None => release,
    })
}

async fn read_observation<T: MicroVmClient>(
    client: &T,
    release: &ReleaseStatus,
) -> Result<Option<ObservedImageRelease>, ClankerError> {
    client
        .inspect_image(InspectImageRequest {
            image_identifier: release.image_arn.clone(),
            image_version: Some(release.image_version.clone()),
        })
        .await
        .map_err(Into::into)
}

fn apply_observation(mut release: ReleaseStatus, observed: ObservedImageRelease) -> ReleaseStatus {
    release.image_state = observed.image_state.as_str().into();
    release.version_state = observed.version_state.as_str().into();
    release.version_status = observed.version_status.as_str().into();
    release.state_reason = observed.state_reason;
    release
}

pub(super) fn pending_release(
    app: &str,
    image_arn: String,
    image_version: &str,
    bundle_digest: Option<String>,
    artifact_uri: Option<String>,
) -> ReleaseStatus {
    ReleaseStatus {
        app: app.into(),
        release: format!("{app}@{image_version}"),
        image_arn,
        image_version: image_version.into(),
        image_state: "PENDING".into(),
        version_state: "PENDING".into(),
        version_status: "INACTIVE".into(),
        state_reason: None,
        bundle_digest,
        artifact_uri,
        build_log_group: format!("/aws/lambda-microvms/{app}"),
    }
}

fn is_ready(release: &ObservedImageRelease) -> bool {
    matches!(
        release.image_state,
        ImageState::Created | ImageState::Updated
    ) && release.version_state == ImageVersionState::Successful
        && release.version_status == ImageVersionStatus::Active
}

fn is_failed(release: &ObservedImageRelease) -> bool {
    let image_in_progress = matches!(
        release.image_state,
        ImageState::Creating | ImageState::Created | ImageState::Updating | ImageState::Updated
    );
    let version_in_progress = matches!(
        release.version_state,
        ImageVersionState::Pending | ImageVersionState::InProgress | ImageVersionState::Successful
    );
    !(image_in_progress && version_in_progress)
}

pub(super) fn duration_or(
    argument: Option<&str>,
    default: Duration,
) -> Result<Duration, ClankerError> {
    let Some(value) = argument else {
        return Ok(default);
    };
    humantime::parse_duration(value).map_err(|error| {
        ClankerError::InvalidConfig(format!("invalid duration `{value}`: {error}"))
    })
}

fn status_text(status: &ReleaseStatus) -> String {
    format!(
        "App:        {}\nRelease:    {}\nImage:      {}\nBuild:      {}\nActivation: {}\nLogs:       {}",
        status.app,
        status.release,
        status.image_state,
        status.version_state,
        status.version_status,
        status.build_log_group
    )
}
