use crate::client::MicroVmClient;
use crate::config::{ProjectConfig, Settings};
use crate::output::{ReleaseProgress, render};
use crate::release::{Release, ReleaseStatus, wait_for_release};
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, Args)]
pub struct StatusOptions {
    /// Release to inspect, in NAME@VERSION form.
    #[arg(value_name = "RELEASE")]
    pub release: Option<String>,
    /// Wait until the release becomes active.
    #[arg(long)]
    pub wait: bool,
    #[command(flatten)]
    pub settings: StatusSettings,
}

/// Status settings, shared by `--flags` and the `[status]` table.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct StatusSettings {
    #[arg(long, value_parser = humantime::parse_duration)]
    #[serde(with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

impl Settings for StatusSettings {}

impl StatusSettings {
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &StatusOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let mut progress = ReleaseProgress::new(format);
    let result = status(options, config, client, |status| progress.report(status)).await?;
    render(format, &result, || {
        let observed = &result.observation;
        format!(
            "Image name:  {}\nRelease:    {}\nImage:      {}\nBuild:      {}\nActivation: {}\nLogs:       {}",
            result.image_name,
            result.release,
            observed.image_state_name(),
            observed.version_state,
            observed.version_status,
            result.build_log_group
        )
    })
}

async fn status<T, F>(
    cli: &StatusOptions,
    config: &ProjectConfig,
    client: &T,
    report: F,
) -> Result<ReleaseStatus, ClankerError>
where
    T: MicroVmClient,
    F: FnMut(&ReleaseStatus),
{
    let settings = config.status.merge(&cli.settings)?;
    let target = config.target(cli.release.as_deref(), config.account_role(&config.run)?)?;
    let mut observed = None;
    let version = if let Some(version) = target.version {
        version
    } else {
        let latest = client
            .observe(&target.arn, None)
            .await?
            .ok_or_else(|| ClankerError::ImageNotFound(target.arn.to_string()))?;
        observed.insert(latest).image_version.clone()
    };
    let release = Release::new(&target.name, target.arn, &version);

    if cli.wait {
        return wait_for_release(client, release, observed, settings.timeout(), report).await;
    }
    let observed = match observed {
        Some(observed) => Some(observed),
        None => client.observe(&release.arn, Some(&release.version)).await?,
    };
    Ok(release.status(observed.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient};
    use crate::test_support::{ROLE, active, project};
    use aws_sdk_lambdamicrovms::types::MicrovmImageVersionStatus;
    use tempfile::TempDir;

    fn config() -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), &format!("[run]\n{ROLE}"));
        (directory, config)
    }

    fn inspected(calls: &[Call]) -> Option<String> {
        let [Call::Observe(_, version)] = calls else {
            panic!("expected one inspection, got {calls:?}");
        };
        version.clone()
    }

    #[tokio::test]
    async fn explicit_release_inspects_the_exact_version() {
        let client = FakeMicroVmClient::default().observed([Ok(Some(active("3")))]);
        let cli = StatusOptions {
            release: Some("demo@3".into()),
            ..StatusOptions::default()
        };
        let (_directory, config) = config();

        let result = status(&cli, &config, &client, |_| {}).await.unwrap();

        assert_eq!(result.release, "demo@3");
        assert_eq!(
            result.observation.version_status,
            MicrovmImageVersionStatus::Active
        );
        assert_eq!(inspected(&client.calls()).as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn latest_release_is_resolved_from_the_image() {
        let client = FakeMicroVmClient::default().observed([Ok(Some(active("2")))]);
        let (_directory, config) = config();

        let result = status(&StatusOptions::default(), &config, &client, |_| {})
            .await
            .unwrap();

        assert_eq!(result.release, "demo@2");
        assert_eq!(
            result.image_arn.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(inspected(&client.calls()), None);
    }

    #[tokio::test]
    async fn a_missing_image_is_reported() {
        let client = FakeMicroVmClient::default();
        let (_directory, config) = config();

        let error = status(&StatusOptions::default(), &config, &client, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::ImageNotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn an_unreported_release_reports_no_image_state() {
        let client = FakeMicroVmClient::default();
        let cli = StatusOptions {
            release: Some("demo@9".into()),
            ..StatusOptions::default()
        };
        let (_directory, config) = config();

        let result = status(&cli, &config, &client, |_| {}).await.unwrap();

        assert_eq!(result.observation.image_state_name(), "UNKNOWN");
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["imageState"], serde_json::Value::Null);
        assert_eq!(json["versionState"], "PENDING");
        assert_eq!(json["versionStatus"], "INACTIVE");
    }

    #[tokio::test]
    async fn waiting_reuses_the_first_observation() {
        let client =
            FakeMicroVmClient::default().observed([Ok(Some(active("4"))), Ok(Some(active("4")))]);
        let cli = StatusOptions {
            release: Some("demo@4".into()),
            wait: true,
            ..StatusOptions::default()
        };
        let (_directory, config) = config();

        let result = status(&cli, &config, &client, |_| {}).await.unwrap();

        assert_eq!(result.release, "demo@4");
        assert_eq!(client.calls().len(), 1);
    }
}
