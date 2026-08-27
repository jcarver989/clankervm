use super::status::{duration_or, pending_release, wait_for_release};
use super::{image_account, render};
use crate::bundle::{ZipBundle, create_zip_bundle};
use crate::client::{
    ImageConfiguration, ImageHooks, MicroVmClient, PruneImageVersionsRequest, PublishImageRequest,
};
use crate::config::ProjectConfig;
use crate::util::non_empty;
use crate::{Arn, ClankerError, OutputFormat, Project, Tags};
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Use an existing ZIP instead of bundling the configured context. This is invocation-only.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
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
    #[arg(long)]
    pub timeout: Option<String>,
}

pub(super) async fn execute<T: MicroVmClient>(
    args: PushArgs,
    project: &Project,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let bundle_path = args.bundle.clone();
    let config = effective_config(args, project.config())?;
    let bundle = load_bundle(bundle_path.as_deref(), &config, project)?;
    let role = Arn::parse(required(
        config.push.build_role_arn.as_deref(),
        "push.build-role-arn",
    )?)?;
    let identifier = Arn::image(
        &config.app.name,
        &config.app.region,
        Some(image_account(&config, None)?),
    )?;
    let artifact_bucket = required(
        config.push.artifact_bucket.as_deref(),
        "push.artifact-bucket",
    )?;
    eprintln!(
        "› Publishing bundle {}",
        &bundle.digest[..bundle.digest.len().min(12)]
    );
    let published = client
        .publish_image(PublishImageRequest {
            image_identifier: identifier.clone(),
            name: config.app.name.clone(),
            bundle: bundle.bytes,
            bundle_digest: bundle.digest.clone(),
            artifact_bucket: artifact_bucket.into(),
            configuration: resolve_image_configuration(&config, &bundle.digest, &role)?,
            tags: Tags::parse(&config.push.tags)?,
        })
        .await?;
    let release = pending_release(
        &config.app.name,
        identifier.clone(),
        &published.image_version,
        Some(bundle.digest),
        Some(published.artifact_uri),
    );
    let result = wait_for_release(client, release, None, config.push.timeout).await?;
    if let Some(keep) = config.push.keep_versions {
        client
            .prune_image_versions(PruneImageVersionsRequest {
                image_identifier: identifier,
                versions_to_keep: keep,
            })
            .await?;
    }
    render(format, &result, || format!("✓ Released {}", result.release))
}

fn effective_config(args: PushArgs, config: &ProjectConfig) -> Result<ProjectConfig, ClankerError> {
    let mut config = config.clone();
    let push = &mut config.push;
    if let Some(value) = args.context {
        push.context = value;
    }
    if let Some(value) = args.artifact_bucket {
        push.artifact_bucket = Some(value);
    }
    if let Some(value) = args.build_role_arn {
        push.build_role_arn = Some(value);
    }
    if let Some(value) = args.base_image {
        push.base_image = Some(value);
    }
    if let Some(value) = args.minimum_memory_mib {
        push.minimum_memory_mib = Some(value);
    }
    if let Some(values) = args.capabilities {
        push.capabilities = values
            .into_iter()
            .map(|value| match value.as_str() {
                "ALL" => Ok(crate::ImageCapability::All),
                _ => Err(ClankerError::InvalidConfig(format!(
                    "unknown image capability `{value}`"
                ))),
            })
            .collect::<Result<_, _>>()?;
    }
    if let Some(value) = args.egress {
        push.egress = Some(value);
    }
    if let Some(value) = args.keep_versions {
        push.keep_versions = Some(value);
    }
    if let Some(values) = args.tags {
        push.tags = values;
    }
    if let Some(value) = args.port {
        push.port = value;
    }
    if let Some(value) = args.ready_timeout_seconds {
        push.ready_timeout_seconds = value;
    }
    if let Some(value) = args.run_timeout_seconds {
        push.run_timeout_seconds = value;
    }
    if let Some(value) = args.terminate_timeout_seconds {
        push.terminate_timeout_seconds = value;
    }
    if let Some(value) = args.timeout {
        push.timeout = duration_or(Some(&value), push.timeout)?;
    }
    Tags::parse(&push.tags)?;
    Ok(config)
}

fn load_bundle(
    path: Option<&std::path::Path>,
    config: &ProjectConfig,
    project: &Project,
) -> Result<ZipBundle, ClankerError> {
    if let Some(path) = path {
        let path = project.resolve(path);
        return ZipBundle::from_path(&path).map_err(|source| ClankerError::Io {
            action: format!("read bundle {}", path.display()),
            source,
        });
    }
    let context = project.resolve(&config.push.context);
    create_zip_bundle(&context).map_err(|source| ClankerError::Io {
        action: format!("create bundle from {}", context.display()),
        source,
    })
}

fn resolve_image_configuration(
    config: &ProjectConfig,
    digest: &str,
    role: &Arn,
) -> Result<ImageConfiguration, ClankerError> {
    Ok(ImageConfiguration {
        base_image_arn: Arn::base_image(
            &config.app.region,
            config.push.base_image.as_deref().unwrap_or("al2023-1"),
        )?,
        build_role_arn: role.clone(),
        description: format!("Bundle {digest}"),
        minimum_memory_mib: config.push.minimum_memory_mib,
        capabilities: config.push.capabilities.clone(),
        egress_network_connector: Arn::network_connector(
            &config.app.region,
            config.push.egress.as_deref().unwrap_or("INTERNET_EGRESS"),
        )?,
        hooks: ImageHooks {
            port: config.push.port,
            ready_timeout_seconds: config.push.ready_timeout_seconds,
            run_timeout_seconds: config.push.run_timeout_seconds,
            terminate_timeout_seconds: config.push.terminate_timeout_seconds,
        },
    })
}

fn required<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, ClankerError> {
    non_empty(value)
        .ok_or_else(|| ClankerError::InvalidConfig(format!("{name} must be configured")))
}
