use crate::bundle::ZipBundle;
use crate::client::{
    ImageCapability, ImageConfiguration, ImageHooks, MicroVmClient, PruneImageVersionsRequest,
    PublishImageRequest,
};
use crate::output::{ReleaseProgress, render};
use crate::release::{Release, ReleaseStatus, release_target, resolve_image, wait_for_release};
use crate::util::{
    deserialize_optional_duration, non_empty_string, parse_duration, required_string,
};
use crate::{Arn, ClankerError, OutputFormat, Project, Tags};
use clap::Args;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_CONTEXT: &str = ".";
const DEFAULT_BASE_IMAGE: &str = "al2023-1";
const DEFAULT_BUILD_EGRESS: &str = "INTERNET_EGRESS";
const DEFAULT_PORT: i32 = 9000;
const DEFAULT_READY_TIMEOUT_SECONDS: i32 = 300;
const DEFAULT_RUN_TIMEOUT_SECONDS: i32 = 60;
const DEFAULT_TERMINATE_TIMEOUT_SECONDS: i32 = 30;

#[derive(Debug, Default, Args)]
pub struct PushArgs {
    /// Select a configured image profile.
    #[arg(long)]
    pub image: Option<String>,
    /// Use an existing ZIP instead of bundling the configured context.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    #[command(flatten)]
    pub config: PushConfig,
}

#[derive(Clone, Debug, Default, Args, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct PushConfig {
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
    #[arg(long = "capability", visible_alias = "capabilities")]
    pub capabilities: Option<Vec<ImageCapability>>,
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
    #[arg(long, value_parser = parse_duration)]
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    pub timeout: Option<Duration>,
}

impl PushConfig {
    pub fn overlay(self, lower: &Self) -> Self {
        Self {
            context: self.context.or_else(|| lower.context.clone()),
            artifact_bucket: self
                .artifact_bucket
                .or_else(|| lower.artifact_bucket.clone()),
            build_role_arn: self.build_role_arn.or_else(|| lower.build_role_arn.clone()),
            base_image: self.base_image.or_else(|| lower.base_image.clone()),
            minimum_memory_mib: self.minimum_memory_mib.or(lower.minimum_memory_mib),
            capabilities: self.capabilities.or_else(|| lower.capabilities.clone()),
            egress: self.egress.or_else(|| lower.egress.clone()),
            keep_versions: self.keep_versions.or(lower.keep_versions),
            tags: self.tags.or_else(|| lower.tags.clone()),
            port: self.port.or(lower.port),
            ready_timeout_seconds: self.ready_timeout_seconds.or(lower.ready_timeout_seconds),
            run_timeout_seconds: self.run_timeout_seconds.or(lower.run_timeout_seconds),
            terminate_timeout_seconds: self
                .terminate_timeout_seconds
                .or(lower.terminate_timeout_seconds),
            timeout: self.timeout.or(lower.timeout),
        }
    }

    fn resolve(self) -> Result<ResolvedPushConfig, ClankerError> {
        let artifact_bucket = required_string(self.artifact_bucket, "push.artifact-bucket")?;
        let build_role_arn = required_string(self.build_role_arn, "push.build-role-arn")?;
        let tags = Tags::parse(self.tags.as_deref().unwrap_or_default())?;
        Ok(ResolvedPushConfig {
            context: self.context.unwrap_or_else(|| DEFAULT_CONTEXT.into()),
            artifact_bucket,
            build_role_arn,
            base_image: non_empty_string(self.base_image, "push.base-image")?
                .unwrap_or_else(|| DEFAULT_BASE_IMAGE.into()),
            minimum_memory_mib: self.minimum_memory_mib,
            capabilities: self.capabilities.unwrap_or_default(),
            egress: non_empty_string(self.egress, "push.egress")?
                .unwrap_or_else(|| DEFAULT_BUILD_EGRESS.into()),
            keep_versions: self.keep_versions,
            tags,
            port: self.port.unwrap_or(DEFAULT_PORT),
            ready_timeout_seconds: self
                .ready_timeout_seconds
                .unwrap_or(DEFAULT_READY_TIMEOUT_SECONDS),
            run_timeout_seconds: self
                .run_timeout_seconds
                .unwrap_or(DEFAULT_RUN_TIMEOUT_SECONDS),
            terminate_timeout_seconds: self
                .terminate_timeout_seconds
                .unwrap_or(DEFAULT_TERMINATE_TIMEOUT_SECONDS),
            timeout: self.timeout.unwrap_or_else(|| Duration::from_hours(1)),
        })
    }
}

#[derive(Clone, Debug)]
struct ResolvedPushConfig {
    context: PathBuf,
    artifact_bucket: String,
    build_role_arn: String,
    base_image: String,
    minimum_memory_mib: Option<i32>,
    capabilities: Vec<ImageCapability>,
    egress: String,
    keep_versions: Option<usize>,
    tags: Tags,
    port: i32,
    ready_timeout_seconds: i32,
    run_timeout_seconds: i32,
    terminate_timeout_seconds: i32,
    timeout: Duration,
}

pub(super) async fn execute<T: MicroVmClient>(
    args: PushArgs,
    project: &Project,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let mut progress = ReleaseProgress::new(format);
    let result = push(args, project, client, |status| progress.report(status)).await?;
    render(format, &result, || format!("✓ Released {}", result.release))
}

async fn push<T: MicroVmClient, U: FnMut(&ReleaseStatus)>(
    args: PushArgs,
    project: &Project,
    client: &T,
    report: U,
) -> Result<ReleaseStatus, ClankerError> {
    let image = resolve_image(&project.config, args.image.as_deref(), None)?;
    let config = args.config.overlay(&image.push).resolve()?;
    let bundle = load_bundle(args.bundle.as_deref(), &config, project)?;
    let role = Arn::parse(&config.build_role_arn)?;
    let identifier = release_target(&image, &config.build_role_arn)?.image_arn;

    eprintln!("› Publishing bundle {}", &bundle.digest[..12]);

    let published = client
        .publish_image(PublishImageRequest {
            image_identifier: identifier.clone(),
            name: image.name.clone(),
            bundle: bundle.bytes,
            bundle_digest: bundle.digest.clone(),
            artifact_bucket: config.artifact_bucket.clone(),
            configuration: resolve_image_configuration(
                &config,
                &image.region,
                &bundle.digest,
                &role,
            )?,
            tags: config.tags.clone(),
        })
        .await?;

    let release = Release::new(
        &image.name,
        identifier.clone(),
        &published.image_version,
        Some(bundle.digest),
        Some(published.artifact_uri),
    );

    let result = wait_for_release(client, release, None, config.timeout, report).await?;
    if let Some(keep) = config.keep_versions {
        client
            .prune_image_versions(PruneImageVersionsRequest {
                image_identifier: identifier,
                versions_to_keep: keep,
            })
            .await?;
    }
    Ok(result)
}

fn load_bundle(
    path: Option<&std::path::Path>,
    config: &ResolvedPushConfig,
    project: &Project,
) -> Result<ZipBundle, ClankerError> {
    if let Some(path) = path {
        let path = project.resolve(path);
        return ZipBundle::from_path(&path).map_err(|source| ClankerError::Io {
            action: format!("read bundle {}", path.display()),
            source,
        });
    }
    let context = project.resolve(&config.context);
    ZipBundle::create(&context).map_err(|source| ClankerError::Io {
        action: format!("create bundle from {}", context.display()),
        source,
    })
}

fn resolve_image_configuration(
    config: &ResolvedPushConfig,
    region: &str,
    digest: &str,
    role: &Arn,
) -> Result<ImageConfiguration, ClankerError> {
    Ok(ImageConfiguration {
        base_image_arn: Arn::base_image(region, &config.base_image)?,
        build_role_arn: role.clone(),
        description: format!("Bundle {digest}"),
        minimum_memory_mib: config.minimum_memory_mib,
        capabilities: config.capabilities.clone(),
        egress_network_connector: Arn::network_connector(region, &config.egress)?,
        hooks: ImageHooks {
            port: config.port,
            ready_timeout_seconds: config.ready_timeout_seconds,
            run_timeout_seconds: config.run_timeout_seconds,
            terminate_timeout_seconds: config.terminate_timeout_seconds,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FakeMicroVmClient, MicroVmCall, MicroVmClientError};
    use crate::test_support::{ObservedImageReleaseBuilder, ProjectBuilder};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn project(directory: &Path) -> Project {
        fs::write(directory.join("app.py"), "print('hi')").unwrap();
        ProjectBuilder::new(directory)
            .push(PushConfig {
                artifact_bucket: Some("artifacts".into()),
                build_role_arn: Some("arn:aws:iam::123456789012:role/build".into()),
                keep_versions: Some(1),
                tags: Some(vec!["team=platform".into()]),
                ..PushConfig::default()
            })
            .build()
    }

    #[test]
    fn config_overlay_uses_higher_priority_values_and_resolves_defaults() {
        let lower = PushConfig {
            context: Some("root".into()),
            port: Some(8000),
            artifact_bucket: Some("bucket".into()),
            build_role_arn: Some("arn:aws:iam::123456789012:role/build".into()),
            ..PushConfig::default()
        };
        let higher = PushConfig {
            context: Some("cli".into()),
            ..PushConfig::default()
        };

        let resolved = higher.overlay(&lower).resolve().unwrap();

        assert_eq!(resolved.context, Path::new("cli"));
        assert_eq!(resolved.port, 8000);
        assert_eq!(resolved.base_image, DEFAULT_BASE_IMAGE);
        assert_eq!(resolved.timeout, Duration::from_hours(1));
    }

    #[test]
    fn invalid_push_values_are_rejected_when_config_is_resolved() {
        for config in [
            PushConfig {
                artifact_bucket: Some(String::new()),
                build_role_arn: Some("arn:aws:iam::123456789012:role/build".into()),
                ..PushConfig::default()
            },
            PushConfig {
                artifact_bucket: Some("bucket".into()),
                build_role_arn: Some(" ".into()),
                ..PushConfig::default()
            },
            PushConfig {
                artifact_bucket: Some("bucket".into()),
                build_role_arn: Some("arn:aws:iam::123456789012:role/build".into()),
                tags: Some(vec!["missing-equals".into()]),
                ..PushConfig::default()
            },
        ] {
            assert!(matches!(
                config.resolve(),
                Err(ClankerError::InvalidConfig(_))
            ));
        }
    }

    #[tokio::test]
    async fn push_publishes_waits_and_prunes() {
        let directory = TempDir::new().unwrap();
        let project = project(directory.path());
        let client = FakeMicroVmClient::builder()
            .inspection_responses([Ok(Some(ObservedImageReleaseBuilder::active("1")))])
            .build();

        let result = push(PushArgs::default(), &project, &client, |_| {})
            .await
            .unwrap();

        assert_eq!(result.release, "demo@1");
        assert_eq!(result.version_status, "ACTIVE");
        let calls = client.calls();
        assert_eq!(calls.len(), 3);
        let MicroVmCall::PublishImage(request) = &calls[0] else {
            panic!("expected publish call, got {calls:?}");
        };
        assert_eq!(
            request.image_identifier.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(
            request
                .tags
                .clone()
                .into_inner()
                .get("team")
                .map(String::as_str),
            Some("platform")
        );
        assert_eq!(request.configuration.hooks.port, 9000);
        assert_eq!(
            request.configuration.description,
            format!("Bundle {}", request.bundle_digest)
        );
        let MicroVmCall::PruneImageVersions(request) = &calls[2] else {
            panic!("expected prune call, got {calls:?}");
        };
        assert_eq!(request.versions_to_keep, 1);
    }

    #[tokio::test]
    async fn publish_failures_surface() {
        let directory = TempDir::new().unwrap();
        let project = project(directory.path());
        let client = FakeMicroVmClient::builder()
            .publish_responses([Err(MicroVmClientError::Service {
                operation: "create image",
                message: "boom".into(),
            })])
            .build();

        let error = push(PushArgs::default(), &project, &client, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
    }

    #[tokio::test]
    async fn prune_failures_surface_after_release() {
        let directory = TempDir::new().unwrap();
        let project = project(directory.path());
        let client = FakeMicroVmClient::builder()
            .inspection_responses([Ok(Some(ObservedImageReleaseBuilder::active("1")))])
            .prune_responses([Err(MicroVmClientError::InvalidVersionsToKeep)])
            .build();

        let error = push(PushArgs::default(), &project, &client, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ClankerError::MicroVmClient(MicroVmClientError::InvalidVersionsToKeep)
        ));
    }
}
