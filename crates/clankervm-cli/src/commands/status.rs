use crate::client::{InspectImageRequest, MicroVmClient, ObservedImageRelease};
use crate::config::ProjectConfig;
use crate::output::{ReleaseProgress, render};
use crate::release::{Release, ReleaseStatus, wait_for_release};
use crate::util::{deserialize_optional_duration, parse_duration};
use crate::{ClankerError, OutputFormat, Project};
use clap::Args;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Default, Args)]
pub struct StatusArgs {
    pub release: Option<String>,
    /// Wait until the release becomes active.
    #[arg(long)]
    pub wait: bool,
    #[command(flatten)]
    pub config: StatusConfig,
}

#[derive(Clone, Debug, Default, Args, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct StatusConfig {
    #[arg(long, value_parser = parse_duration)]
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    pub timeout: Option<Duration>,
}

impl StatusConfig {
    pub fn overlay(self, lower: &Self) -> Self {
        Self {
            timeout: self.timeout.or(lower.timeout),
        }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout.unwrap_or_else(|| Duration::from_hours(1))
    }
}

pub(super) async fn execute<T: MicroVmClient>(
    args: StatusArgs,
    project: &Project,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let mut progress = ReleaseProgress::new(format);
    let result = status(args, &project.config, client, |status| {
        progress.report(status);
    })
    .await?;
    render(format, &result, || {
        format!(
            "Image name:  {}\nRelease:    {}\nImage:      {}\nBuild:      {}\nActivation: {}\nLogs:       {}",
            result.image_name,
            result.release,
            result.image_state,
            result.version_state,
            result.version_status,
            result.build_log_group
        )
    })
}

async fn status<T, F>(
    args: StatusArgs,
    config: &ProjectConfig,
    client: &T,
    report: F,
) -> Result<ReleaseStatus, ClankerError>
where
    T: MicroVmClient,
    F: FnMut(&ReleaseStatus),
{
    let (release, observed) = resolve_release(args.release.as_deref(), config, client).await?;
    if args.wait {
        let timeout = args.config.overlay(&config.status).timeout();
        return wait_for_release(client, release, observed, timeout, report).await;
    }
    let observed = match observed {
        Some(observed) => Some(observed),
        None => client.inspect_image(release.inspect_request()).await?,
    };
    Ok(release.status(observed.as_ref()))
}

async fn resolve_release<T: MicroVmClient>(
    requested: Option<&str>,
    config: &ProjectConfig,
    client: &T,
) -> Result<(Release, Option<ObservedImageRelease>), ClankerError> {
    let image = config.resolve_image(requested)?;
    let account_role = image.configured_account_role()?;
    let target = image.target(account_role)?;
    if let Some(version) = target.version {
        return Ok((
            Release::new(&target.name, target.image_arn, &version, None, None),
            None,
        ));
    }
    let observed = client
        .inspect_image(InspectImageRequest {
            image_identifier: target.image_arn.clone(),
            image_version: None,
        })
        .await?
        .ok_or_else(|| ClankerError::InvalidImage(target.image_arn.to_string()))?;
    let release = Release::new(
        &target.name,
        target.image_arn,
        &observed.image_version,
        None,
        None,
    );
    Ok((release, Some(observed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FakeMicroVmClient, MicroVmCall};
    use crate::commands::RunConfig;
    use crate::test_support::{ObservedImageReleaseBuilder, ProjectConfigBuilder};

    fn config() -> ProjectConfig {
        ProjectConfigBuilder::new()
            .run(RunConfig {
                execution_role_arn: Some("arn:aws:iam::123456789012:role/run".into()),
                ..RunConfig::default()
            })
            .build()
    }

    #[tokio::test]
    async fn explicit_release_inspects_the_exact_version() {
        let client = FakeMicroVmClient::builder()
            .inspection_responses([Ok(Some(ObservedImageReleaseBuilder::active("3")))])
            .build();
        let args = StatusArgs {
            release: Some("demo@3".into()),
            ..StatusArgs::default()
        };

        let result = status(args, &config(), &client, |_| {}).await.unwrap();

        assert_eq!(result.release, "demo@3");
        assert_eq!(result.version_status, "ACTIVE");
        let calls = client.calls();
        let [MicroVmCall::InspectImage(request)] = calls.as_slice() else {
            panic!("expected one inspection, got {calls:?}");
        };
        assert_eq!(request.image_version.as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn latest_release_is_resolved_from_the_image() {
        let client = FakeMicroVmClient::builder()
            .inspection_responses([Ok(Some(ObservedImageReleaseBuilder::active("2")))])
            .build();

        let result = status(StatusArgs::default(), &config(), &client, |_| {})
            .await
            .unwrap();

        assert_eq!(result.release, "demo@2");
        assert_eq!(
            result.image_arn.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(client.calls().len(), 1);
    }
}
