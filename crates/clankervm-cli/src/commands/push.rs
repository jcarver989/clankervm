use crate::artifact::Artifact;
use crate::client::MicroVmClient;
use crate::config::{ProjectConfig, Settings};
use crate::output::{ReleaseProgress, render};
use crate::release::{Release, ReleaseStatus, wait_for_release};
use crate::util::{parse_key_values, required, validate_non_empty};
use crate::{ClankerError, OutputFormat};
use aws_sdk_lambdamicrovms::types::Capability;
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_CONTEXT: &str = ".";
const DEFAULT_BASE_IMAGE: &str = "al2023-1";
const DEFAULT_BUILD_EGRESS: &str = "INTERNET_EGRESS";
const DEFAULT_PORT: i32 = 9000;
const DEFAULT_READY_TIMEOUT_SECONDS: i32 = 300;
const DEFAULT_RUN_TIMEOUT_SECONDS: i32 = 60;
const DEFAULT_TERMINATE_TIMEOUT_SECONDS: i32 = 30;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, Args)]
pub struct PushOptions {
    /// Prepared directory to ZIP or an existing ZIP file. Overrides push.context.
    #[arg(value_name = "PATH")]
    pub source: Option<PathBuf>,
    #[command(flatten)]
    pub settings: PushSettings,
}

/// Push settings, shared by `--flags` and the `[push]` table.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct PushSettings {
    #[arg(long)]
    pub context: Option<PathBuf>,
    #[arg(long)]
    pub artifact_bucket: Option<String>,
    #[arg(long)]
    pub build_role_arn: Option<String>,
    #[arg(long)]
    pub base_image: Option<String>,
    #[arg(long)]
    pub minimum_memory_mib: Option<i32>,
    /// Image capability; repeat for multiple capabilities.
    #[arg(long = "capability", visible_alias = "capabilities", value_parser = validate_capability)]
    pub capabilities: Option<Vec<String>>,
    #[arg(long)]
    pub egress: Option<String>,
    #[arg(long)]
    pub keep_versions: Option<usize>,
    /// Image tag in key=value form; repeat for multiple tags.
    #[arg(long = "tag")]
    pub tags: Option<Vec<String>>,
    #[arg(long)]
    pub port: Option<i32>,
    #[arg(long)]
    pub ready_timeout_seconds: Option<i32>,
    #[arg(long)]
    pub run_timeout_seconds: Option<i32>,
    #[arg(long)]
    pub terminate_timeout_seconds: Option<i32>,
    #[arg(long, value_parser = humantime::parse_duration)]
    #[serde(with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

impl Settings for PushSettings {
    fn validate(&self) -> Result<(), ClankerError> {
        validate_non_empty(self.artifact_bucket.as_deref(), "push.artifact-bucket")?;
        validate_non_empty(self.build_role_arn.as_deref(), "push.build-role-arn")?;
        validate_non_empty(self.base_image.as_deref(), "push.base-image")?;
        validate_non_empty(self.egress.as_deref(), "push.egress")?;
        self.tags()?;
        self.capabilities()?;
        Ok(())
    }
}

impl PushSettings {
    pub(crate) fn artifact_bucket(&self) -> Result<&str, ClankerError> {
        required(self.artifact_bucket.as_deref(), "push.artifact-bucket")
    }

    pub(crate) fn build_role_arn(&self) -> Result<&str, ClankerError> {
        required(self.build_role_arn.as_deref(), "push.build-role-arn")
    }

    pub(crate) fn tags(&self) -> Result<BTreeMap<String, String>, ClankerError> {
        parse_key_values(self.tags.as_deref().unwrap_or_default(), "tag", true)
    }

    pub(crate) fn capabilities(&self) -> Result<Vec<Capability>, ClankerError> {
        self.capabilities
            .iter()
            .flatten()
            .map(|name| parse_capability(name).map_err(ClankerError::InvalidConfig))
            .collect()
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }
    pub(crate) fn base_image(&self) -> &str {
        self.base_image.as_deref().unwrap_or(DEFAULT_BASE_IMAGE)
    }
    pub(crate) fn egress(&self) -> &str {
        self.egress.as_deref().unwrap_or(DEFAULT_BUILD_EGRESS)
    }
    pub(crate) fn port(&self) -> i32 {
        self.port.unwrap_or(DEFAULT_PORT)
    }
    pub(crate) fn ready_timeout_seconds(&self) -> i32 {
        self.ready_timeout_seconds
            .unwrap_or(DEFAULT_READY_TIMEOUT_SECONDS)
    }
    pub(crate) fn run_timeout_seconds(&self) -> i32 {
        self.run_timeout_seconds
            .unwrap_or(DEFAULT_RUN_TIMEOUT_SECONDS)
    }
    pub(crate) fn terminate_timeout_seconds(&self) -> i32 {
        self.terminate_timeout_seconds
            .unwrap_or(DEFAULT_TERMINATE_TIMEOUT_SECONDS)
    }
}

/// Keeps the spelling a user typed while rejecting capabilities AWS does not have.
fn validate_capability(value: &str) -> Result<String, String> {
    parse_capability(value).map(|_| value.to_owned())
}

fn parse_capability(value: &str) -> Result<Capability, String> {
    Capability::try_parse(value).map_err(|_| format!("unknown image capability `{value}`"))
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &PushOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let mut progress = ReleaseProgress::new(format);
    let result = push(options, config, client, |status| progress.report(status)).await?;
    render(format, &result, || format!("✓ Released {}", result.release))
}

async fn push<T, F>(
    options: &PushOptions,
    config: &ProjectConfig,
    client: &T,
    report: F,
) -> Result<ReleaseStatus, ClankerError>
where
    T: MicroVmClient,
    F: FnMut(&ReleaseStatus),
{
    let settings = config.push.merge(&options.settings)?;
    let path = config.resolve(
        options
            .source
            .as_deref()
            .or(settings.context.as_deref())
            .unwrap_or(Path::new(DEFAULT_CONTEXT)),
    );
    let bundle = Artifact::load(&path).map_err(|source| ClankerError::Io {
        action: format!("load artifact from {}", path.display()),
        source,
    })?;
    let spec = config.image_spec(&settings, &bundle.digest)?;

    eprintln!("› Publishing artifact {}", &bundle.digest[..12]);

    let published = client.publish(&spec, &bundle).await?;
    let release = Release::new(&spec.name, spec.arn.clone(), &published.version)
        .with_artifact(bundle.digest, published.artifact_uri);

    let status = wait_for_release(client, release, None, settings.timeout(), report).await?;
    if let Some(keep) = settings.keep_versions {
        client.prune(&spec.arn, keep).await?;
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient, MicroVmClientError};
    use crate::test_support::{self, active};
    use aws_sdk_lambdamicrovms::types::MicrovmImageVersionStatus;
    use std::fs;
    use tempfile::TempDir;

    /// A real project file plus a source directory to bundle.
    fn project(directory: &Path) -> ProjectConfig {
        fs::write(directory.join("app.py"), "print('hi')").unwrap();
        test_support::project(
            directory,
            "[push]\nartifact-bucket = \"artifacts\"\nbuild-role-arn = \"arn:aws:iam::123456789012:role/build\"\nkeep-versions = 1\ntags = [\"team=platform\"]\n",
        )
    }

    fn active_client() -> FakeMicroVmClient {
        FakeMicroVmClient::default().observed([Ok(Some(active("1")))])
    }

    #[test]
    fn push_defaults_apply_once_values_are_resolved() {
        let settings = PushSettings::default();

        assert_eq!(settings.base_image(), DEFAULT_BASE_IMAGE);
        assert_eq!(settings.egress(), DEFAULT_BUILD_EGRESS);
        assert_eq!(settings.port(), DEFAULT_PORT);
        assert_eq!(settings.timeout(), DEFAULT_TIMEOUT);
    }

    #[test]
    fn invalid_push_values_are_rejected() {
        for settings in [
            PushSettings {
                artifact_bucket: Some(String::new()),
                ..PushSettings::default()
            },
            PushSettings {
                base_image: Some(" ".into()),
                ..PushSettings::default()
            },
            PushSettings {
                tags: Some(vec!["missing-equals".into()]),
                ..PushSettings::default()
            },
            PushSettings {
                capabilities: Some(vec!["NOPE".into()]),
                ..PushSettings::default()
            },
        ] {
            assert!(
                matches!(settings.validate(), Err(ClankerError::InvalidConfig(_))),
                "accepted {settings:?}"
            );
        }
    }

    #[tokio::test]
    async fn push_publishes_waits_and_prunes() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path());
        let digest = Artifact::load(directory.path()).unwrap().digest;
        let client = active_client();

        let result = push(&PushOptions::default(), &config, &client, |_| {})
            .await
            .unwrap();

        assert_eq!(result.release, "demo@1");
        assert_eq!(
            result.observation.version_status,
            MicrovmImageVersionStatus::Active
        );
        let calls = client.calls();
        let [
            Call::Publish(spec),
            Call::Observe(..),
            Call::Prune(arn, keep),
        ] = calls.as_slice()
        else {
            panic!("expected publish, observe and prune, got {calls:?}");
        };
        assert_eq!(
            spec.arn.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(spec.tags.get("team").map(String::as_str), Some("platform"));
        assert_eq!(spec.configuration.hooks.port(), Some(9000));
        assert_eq!(spec.configuration.description, format!("Bundle {digest}"));
        assert_eq!(
            arn.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(*keep, 1);
    }

    #[tokio::test]
    async fn push_uploads_a_prebuilt_zip_without_repacking_it() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path());
        let zip_path = directory.path().join("image.zip");
        let expected = Artifact::load(directory.path()).unwrap().bytes;
        fs::write(&zip_path, &expected).unwrap();
        let client = active_client();
        let options = PushOptions {
            source: Some(zip_path),
            ..PushOptions::default()
        };

        push(&options, &config, &client, |_| {}).await.unwrap();

        let calls = client.calls();
        let Call::Publish(spec) = &calls[0] else {
            panic!("expected publish, got {calls:?}");
        };
        assert_eq!(
            spec.configuration.description,
            format!("Bundle {}", crate::util::sha256_hex(&expected))
        );
    }

    #[tokio::test]
    async fn publish_failures_surface() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path());
        let client = FakeMicroVmClient::default().published([Err(MicroVmClientError::Service {
            operation: "create image",
            message: "boom".into(),
        })]);

        let error = push(&PushOptions::default(), &config, &client, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
    }

    #[tokio::test]
    async fn prune_failures_surface_after_release() {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path());
        let client = active_client().pruned([Err(MicroVmClientError::InvalidVersionsToKeep)]);

        let error = push(&PushOptions::default(), &config, &client, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ClankerError::MicroVmClient(MicroVmClientError::InvalidVersionsToKeep)
        ));
    }
}
