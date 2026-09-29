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
const DEFAULT_ARTIFACT_PREFIX: &str = "clankervm";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, Args)]
pub struct PushOptions {
    /// Prepared directory to ZIP or an existing ZIP file. Overrides microvm.image.artifact.source.
    #[arg(value_name = "PATH")]
    pub source: Option<PathBuf>,
    #[command(flatten)]
    pub settings: PushSettings,
}

/// Push settings resolved from the project file and CLI flags.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct PushSettings {
    #[arg(long)]
    pub context: Option<PathBuf>,
    #[arg(long)]
    pub artifact_bucket: Option<String>,
    #[arg(long)]
    pub artifact_prefix: Option<String>,
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
    /// How many times to publish a new version after a build fails.
    #[arg(long)]
    pub build_retries: Option<usize>,
    /// Image tag in key=value form; repeat for multiple tags.
    #[arg(long = "tag")]
    pub tags: Option<Vec<String>>,
    #[arg(long)]
    pub port: Option<i32>,
    #[arg(long)]
    pub ready_timeout_seconds: Option<i32>,
    #[arg(long, value_parser = clap::value_parser!(i32).range(1..=3600))]
    pub validate_timeout_seconds: Option<i32>,
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
        validate_non_empty(
            self.artifact_bucket.as_deref(),
            "microvm.image.artifact.s3-bucket",
        )?;
        validate_non_empty(
            self.artifact_prefix.as_deref(),
            "microvm.image.artifact.s3-prefix",
        )?;
        if self.artifact_prefix().is_empty() {
            return Err(ClankerError::InvalidConfig(
                "microvm.image.artifact.s3-prefix must name at least one key segment".into(),
            ));
        }
        validate_non_empty(self.build_role_arn.as_deref(), "microvm.image.iam-role")?;
        validate_non_empty(self.base_image.as_deref(), "microvm.image.base-image")?;
        validate_non_empty(self.egress.as_deref(), "microvm.image.network.egress")?;
        if self.keep_versions == Some(0) {
            return Err(ClankerError::InvalidConfig(
                "microvm.image.versions.max must be at least 1".into(),
            ));
        }
        if self
            .validate_timeout_seconds
            .is_some_and(|seconds| !(1..=3600).contains(&seconds))
        {
            return Err(ClankerError::InvalidConfig(
                "microvm.image.hooks.validate-timeout must be between 1s and 1h".into(),
            ));
        }
        self.tags()?;
        self.capabilities()?;
        Ok(())
    }
}

impl PushSettings {
    pub(crate) fn artifact_bucket(&self) -> Result<&str, ClankerError> {
        required(
            self.artifact_bucket.as_deref(),
            "microvm.image.artifact.s3-bucket",
        )
    }

    /// The key prefix bundles are uploaded under, without a trailing slash.
    pub(crate) fn artifact_prefix(&self) -> &str {
        self.artifact_prefix
            .as_deref()
            .unwrap_or(DEFAULT_ARTIFACT_PREFIX)
            .trim_end_matches('/')
    }

    pub(crate) fn build_role_arn(&self) -> Result<&str, ClankerError> {
        required(self.build_role_arn.as_deref(), "microvm.image.iam-role")
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
    pub(crate) fn build_retries(&self) -> usize {
        self.build_retries.unwrap_or_default()
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
    pub(crate) fn validate_timeout_seconds(&self) -> i32 {
        self.validate_timeout_seconds.unwrap_or(300)
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

async fn push<T: MicroVmClient, U: FnMut(&ReleaseStatus)>(
    options: &PushOptions,
    config: &ProjectConfig,
    client: &T,
    mut report: U,
) -> Result<ReleaseStatus, ClankerError> {
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

    let mut retries_left = settings.build_retries();
    loop {
        if let Some(max) = settings.keep_versions {
            client.prune(&spec.arn, max - 1).await?;
        }
        let published = client.publish(&spec, &bundle).await?;
        let release = Release::new(&spec.name, spec.arn.clone(), &published.version)
            .with_artifact(bundle.digest.clone(), published.artifact_uri);

        match wait_for_release(client, release, None, settings.timeout(), &mut report).await {
            Err(ClankerError::ReleaseFailed {
                release, reason, ..
            }) if retries_left > 0 => {
                retries_left -= 1;
                eprintln!("› {release} failed: {reason}; retrying ({retries_left} retries left)");
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient, MicroVmClientError, Observation, Published};
    use crate::test_support::{self, active, failed};
    use aws_sdk_lambdamicrovms::types::MicrovmImageVersionStatus;
    use std::error::Error;
    use std::fs;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn push_defaults_apply_once_values_are_resolved() {
        let settings = PushSettings::default();

        assert_eq!(settings.base_image(), DEFAULT_BASE_IMAGE);
        assert_eq!(settings.egress(), DEFAULT_BUILD_EGRESS);
        assert_eq!(settings.port(), DEFAULT_PORT);
        assert_eq!(settings.artifact_prefix(), DEFAULT_ARTIFACT_PREFIX);
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
                artifact_prefix: Some(String::new()),
                ..PushSettings::default()
            },
            PushSettings {
                artifact_prefix: Some("/".into()),
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
            PushSettings {
                keep_versions: Some(0),
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
    async fn push_prunes_publishes_and_waits() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let digest = Artifact::load(directory.path())?.digest;
        let client = active_client();

        let result = push(&PushOptions::default(), &config, &client, |_| {}).await?;

        assert_eq!(result.release, "demo@1");
        assert_eq!(
            result.observation.version_status,
            MicrovmImageVersionStatus::Active
        );
        let calls = client.calls();
        let [
            Call::Prune(arn, keep),
            Call::Publish(spec),
            Call::Observe(..),
        ] = calls.as_slice()
        else {
            return Err(format!("expected prune, publish and observe, got {calls:?}").into());
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
        assert_eq!(
            *keep, 0,
            "one below the maximum leaves room for the new version"
        );
        Ok(())
    }

    #[tokio::test]
    async fn push_uploads_a_prebuilt_zip_without_repacking_it() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let zip_path = directory.path().join("image.zip");
        let expected = Artifact::load(directory.path())?.bytes;
        fs::write(&zip_path, &expected)?;
        let client = active_client();
        let options = PushOptions {
            source: Some(zip_path),
            ..PushOptions::default()
        };

        push(&options, &config, &client, |_| {}).await?;

        let calls = client.calls();
        let Some(Call::Publish(spec)) = calls.get(1) else {
            return Err(format!("expected publish after prune, got {calls:?}").into());
        };
        assert_eq!(
            spec.configuration.description,
            format!("Bundle {}", crate::util::sha256_hex(&expected))
        );
        Ok(())
    }

    #[tokio::test]
    async fn push_uploads_under_the_configured_artifact_prefix() -> TestResult {
        let directory = TempDir::new()?;
        fs::write(directory.path().join("app.py"), "print('hi')")?;
        let config = test_support::project(
            directory.path(),
            "[microvm.image]\niam-role = \"arn:aws:iam::123456789012:role/build\"\n[microvm.image.artifact]\ns3-bucket = \"artifacts\"\ns3-prefix = \"employee-clanker/\"\n",
        );
        let client = active_client();

        push(&PushOptions::default(), &config, &client, |_| {}).await?;

        let calls = client.calls();
        let Some(Call::Publish(spec)) = calls.first() else {
            return Err(format!("expected publish, got {calls:?}").into());
        };
        assert_eq!(spec.artifact_prefix, "employee-clanker");
        assert_eq!(
            spec.artifact_key("abc"),
            "employee-clanker/demo/bundles/abc.zip"
        );
        Ok(())
    }

    #[tokio::test]
    async fn publish_failures_surface() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let client = FakeMicroVmClient::default().published([Err(service_error())]);

        let error = push(&PushOptions::default(), &config, &client, |_| {})
            .await
            .err()
            .ok_or("expected push to fail")?;

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn prune_failures_stop_the_push_before_publishing() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let client = active_client().pruned([Err(service_error())]);

        let error = push(&PushOptions::default(), &config, &client, |_| {})
            .await
            .err()
            .ok_or("expected push to fail")?;

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
        assert!(matches!(client.calls().as_slice(), [Call::Prune(..)]));
        Ok(())
    }

    #[tokio::test]
    async fn push_publishes_a_new_version_after_a_failed_build() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let client = FakeMicroVmClient::default()
            .published(published(&["1", "2"]))
            .observed([Ok(Some(failed("1"))), Ok(Some(active("2")))]);

        let result = push(&retrying(1), &config, &client, |_| {}).await?;

        assert_eq!(result.release, "demo@2");
        let calls = client.calls();
        let [
            Call::Prune(_, 0),
            Call::Publish(_),
            Call::Observe(_, Some(first)),
            Call::Prune(_, 0),
            Call::Publish(_),
            Call::Observe(_, Some(second)),
        ] = calls.as_slice()
        else {
            return Err(format!(
                "expected each attempt to prune, publish and observe, got {calls:?}"
            )
            .into());
        };
        assert_eq!((first.as_str(), second.as_str()), ("1", "2"));
        Ok(())
    }

    #[tokio::test]
    async fn push_reports_the_last_failed_build_once_retries_run_out() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let last = Observation {
            state_reason: Some("Ready hook invocation timed out".into()),
            ..failed("2")
        };
        let client = FakeMicroVmClient::default()
            .published(published(&["1", "2"]))
            .observed([Ok(Some(failed("1"))), Ok(Some(last))]);

        let error = push(&retrying(1), &config, &client, |_| {})
            .await
            .err()
            .ok_or("expected push to fail")?;

        let ClankerError::ReleaseFailed {
            release, reason, ..
        } = error
        else {
            return Err(format!("expected release failure, got {error}").into());
        };
        assert_eq!(release, "demo@2");
        assert_eq!(reason, "Ready hook invocation timed out");
        Ok(())
    }

    #[tokio::test]
    async fn push_retries_only_failed_builds() -> TestResult {
        let directory = TempDir::new()?;
        let config = project(directory.path())?;
        let client = FakeMicroVmClient::default().observed([Err(service_error())]);

        let error = push(&retrying(1), &config, &client, |_| {})
            .await
            .err()
            .ok_or("expected push to fail")?;

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
        let publishes = client
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::Publish(_)))
            .count();
        assert_eq!(publishes, 1);
        Ok(())
    }

    fn project(directory: &Path) -> std::io::Result<ProjectConfig> {
        fs::write(directory.join("app.py"), "print('hi')")?;
        Ok(test_support::project(
            directory,
            "[microvm.image]\niam-role = \"arn:aws:iam::123456789012:role/build\"\ntags = [\"team=platform\"]\n[microvm.image.artifact]\ns3-bucket = \"artifacts\"\n[microvm.image.versions]\nmax = 1\n",
        ))
    }

    fn active_client() -> FakeMicroVmClient {
        FakeMicroVmClient::default().observed([Ok(Some(active("1")))])
    }

    fn retrying(build_retries: usize) -> PushOptions {
        PushOptions {
            settings: PushSettings {
                build_retries: Some(build_retries),
                ..PushSettings::default()
            },
            ..PushOptions::default()
        }
    }

    fn published(versions: &[&str]) -> Vec<Result<Published, MicroVmClientError>> {
        versions
            .iter()
            .map(|version| {
                Ok(Published {
                    version: (*version).into(),
                    artifact_uri: "s3://artifacts/demo.zip".into(),
                })
            })
            .collect()
    }

    fn service_error() -> MicroVmClientError {
        MicroVmClientError::Service {
            operation: "get image",
            message: "boom".into(),
        }
    }
}
