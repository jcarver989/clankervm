mod artifact;
mod runtime;

use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_sdk_lambdamicrovms::error::SdkError;
use aws_sdk_lambdamicrovms::operation::{
    create_microvm_image::CreateMicrovmImageError, get_microvm_image::GetMicrovmImageError,
    run_microvm::RunMicrovmError, update_microvm_image::UpdateMicrovmImageError,
};
use aws_sdk_lambdamicrovms::types::MicrovmImageState;
use aws_sdk_s3::operation::put_object::PutObjectError;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use thiserror::Error;

pub use artifact::{ContextArtifact, zip_context};
pub use runtime::{
    EnvironmentError, PayloadError, RuntimeEnvironment, build_run_payload, parse_env_assignments,
    parse_env_file,
};

#[derive(Debug, Parser)]
#[command(
    name = "clankervm",
    about = "Build and run generic Lambda MicroVM images"
)]
pub struct Cli {
    #[arg(long, global = true, env = "AWS_REGION")]
    pub region: Option<String>,
    #[arg(long, global = true, env = "AWS_ACCOUNT_ID")]
    pub account_id: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build a Lambda `MicroVM` image from a context.
    Build(BuildArgs),
    /// Run a command in a Lambda `MicroVM` image.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    #[arg(short, long, value_parser = parse_image_name)]
    pub tag: String,
    #[arg(long, env = "CLANKERVM_ARTIFACT_BUCKET")]
    pub artifact_bucket: String,
    #[arg(long, env = "CLANKERVM_BUILD_ROLE_ARN")]
    pub build_role_arn: String,
    #[arg(long, default_value = "al2023-1", env = "CLANKERVM_BASE_IMAGE")]
    pub base_image: String,
    #[arg(
        long,
        default_value = "INTERNET_EGRESS",
        env = "CLANKERVM_BUILD_EGRESS"
    )]
    pub egress: String,
    #[arg(long, default_value_t = 9000, env = "CLANKERVM_HOOK_PORT", value_parser = clap::value_parser!(i32).range(1..=65_535))]
    pub hook_port: i32,
    #[arg(long, default_value_t = 300, env = "CLANKERVM_READY_TIMEOUT", value_parser = clap::value_parser!(i32).range(1..=3_600))]
    pub ready_timeout: i32,
    #[arg(long, default_value_t = 60, env = "CLANKERVM_RUN_TIMEOUT", value_parser = clap::value_parser!(i32).range(1..=60))]
    pub run_timeout: i32,
    #[arg(long, default_value_t = 30, env = "CLANKERVM_TERMINATE_TIMEOUT", value_parser = clap::value_parser!(i32).range(1..=60))]
    pub terminate_timeout: i32,
    #[arg(long)]
    pub no_wait: bool,
    pub context: PathBuf,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, env = "CLANKERVM_EXECUTION_ROLE_ARN")]
    pub execution_role_arn: String,
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    #[arg(short = 'e', long = "env", action = clap::ArgAction::Append)]
    pub env: Vec<String>,
    #[arg(long, default_value = "/bin/sh", env = "CLANKERVM_SHELL")]
    pub shell: String,
    #[arg(long)]
    pub script: Option<PathBuf>,
    #[arg(long, default_value = "NO_INGRESS", env = "CLANKERVM_RUN_INGRESS")]
    pub ingress: String,
    #[arg(long, default_value = "INTERNET_EGRESS", env = "CLANKERVM_RUN_EGRESS")]
    pub egress: String,
    #[arg(long, default_value_t = 3600, env = "CLANKERVM_MAX_DURATION", value_parser = clap::value_parser!(i32).range(1..=28_800))]
    pub max_duration: i32,
    #[arg(long, env = "CLANKERVM_LOG_GROUP")]
    pub log_group: Option<String>,
    pub image: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ClankerError {
    #[error("environment variable {0} must be set")]
    MissingEnvironment(&'static str),
    #[error("invalid image `{0}`: image names require --account-id or AWS_ACCOUNT_ID")]
    InvalidImage(String),
    #[error("failed to {action}: {source}")]
    Io {
        action: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid environment: {0}")]
    Environment(#[from] EnvironmentError),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error("failed to serialize payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to upload artifact: {0}")]
    Upload(#[source] Box<SdkError<PutObjectError>>),
    #[error("failed to get image: {0}")]
    Get(#[source] Box<SdkError<GetMicrovmImageError>>),
    #[error("failed to create image: {0}")]
    Create(#[source] Box<SdkError<CreateMicrovmImageError>>),
    #[error("failed to update image: {0}")]
    Update(#[source] Box<SdkError<UpdateMicrovmImageError>>),
    #[error("failed to run MicroVM: {0}")]
    Run(#[source] Box<SdkError<RunMicrovmError>>),
    #[error("image build finished in state {state}")]
    ImageBuildFailed { state: String },
}

pub async fn execute(cli: Cli) -> Result<(), ClankerError> {
    let region = cli
        .region
        .ok_or(ClankerError::MissingEnvironment("AWS_REGION"))?;
    let sdk = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region.clone()))
        .load()
        .await;
    match cli.command {
        Command::Build(args) => build(args, &region, cli.account_id.as_deref(), &sdk).await,
        Command::Run(args) => runtime::run(args, &region, cli.account_id.as_deref(), &sdk).await,
    }
}

pub fn image_arn(
    image: &str,
    region: &str,
    account_id: Option<&str>,
) -> Result<String, ClankerError> {
    if image.starts_with("arn:") {
        return Ok(image.to_owned());
    }
    if image.is_empty() {
        return Err(ClankerError::InvalidImage(image.into()));
    }
    let account_id = account_id.ok_or_else(|| ClankerError::InvalidImage(image.into()))?;
    Ok(format!(
        "arn:aws:lambda:{region}:{account_id}:microvm-image:{image}"
    ))
}

async fn build(
    args: BuildArgs,
    region: &str,
    account_id: Option<&str>,
    sdk: &SdkConfig,
) -> Result<(), ClankerError> {
    let artifact = zip_context(&args.context).map_err(|source| ClankerError::Io {
        action: "zip build context".into(),
        source,
    })?;
    let digest = artifact.digest;
    let key = format!("clankervm/artifacts/{}-{digest}.zip", args.tag);
    let s3 = aws_sdk_s3::Client::new(sdk);
    s3.put_object()
        .bucket(&args.artifact_bucket)
        .key(&key)
        .body(aws_sdk_s3::primitives::ByteStream::from(artifact.bytes))
        .send()
        .await
        .map_err(|e| ClankerError::Upload(Box::new(e)))?;
    let lambda = aws_sdk_lambdamicrovms::Client::new(sdk);
    let identifier = image_arn(&args.tag, region, account_id)?;
    let found = match lambda
        .get_microvm_image()
        .image_identifier(&identifier)
        .send()
        .await
    {
        Ok(_) => true,
        Err(e)
            if e.as_service_error()
                .is_some_and(GetMicrovmImageError::is_resource_not_found_exception) =>
        {
            false
        }
        Err(e) => return Err(ClankerError::Get(Box::new(e))),
    };
    let output_identifier = identifier.clone();
    let request = ImageRequest {
        identifier,
        name: args.tag,
        uri: format!("s3://{}/{key}", args.artifact_bucket),
        base: base_image_arn(region, &args.base_image),
        role: args.build_role_arn,
        egress: connector(region, &args.egress),
    };
    if found {
        lambda
            .update_microvm_image()
            .image_identifier(request.identifier)
            .code_artifact(aws_sdk_lambdamicrovms::types::CodeArtifact::Uri(
                request.uri,
            ))
            .base_image_arn(request.base)
            .build_role_arn(request.role)
            .egress_network_connectors(request.egress)
            .hooks(artifact::hooks(
                args.hook_port,
                args.ready_timeout,
                args.run_timeout,
                args.terminate_timeout,
            ))
            .send()
            .await
            .map_err(|e| ClankerError::Update(Box::new(e)))?;
    } else {
        lambda
            .create_microvm_image()
            .name(request.name)
            .code_artifact(aws_sdk_lambdamicrovms::types::CodeArtifact::Uri(
                request.uri,
            ))
            .base_image_arn(request.base)
            .build_role_arn(request.role)
            .egress_network_connectors(request.egress)
            .hooks(artifact::hooks(
                args.hook_port,
                args.ready_timeout,
                args.run_timeout,
                args.terminate_timeout,
            ))
            .send()
            .await
            .map_err(|e| ClankerError::Create(Box::new(e)))?;
    }
    if !args.no_wait {
        wait_for_image(&lambda, &output_identifier).await?;
    }
    println!("{output_identifier}");
    Ok(())
}

async fn wait_for_image(
    client: &aws_sdk_lambdamicrovms::Client,
    image_identifier: &str,
) -> Result<(), ClankerError> {
    loop {
        let output = client
            .get_microvm_image()
            .image_identifier(image_identifier)
            .send()
            .await
            .map_err(|error| ClankerError::Get(Box::new(error)))?;
        match output.state() {
            MicrovmImageState::Created | MicrovmImageState::Updated => return Ok(()),
            MicrovmImageState::Creating | MicrovmImageState::Updating => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            state => {
                return Err(ClankerError::ImageBuildFailed {
                    state: state.as_str().into(),
                });
            }
        }
    }
}

struct ImageRequest {
    identifier: String,
    name: String,
    uri: String,
    base: String,
    role: String,
    egress: String,
}
fn connector(region: &str, connector: &str) -> String {
    if connector.starts_with("arn:") {
        connector.to_owned()
    } else {
        format!("arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:{connector}")
    }
}

fn parse_image_name(value: &str) -> Result<String, String> {
    if value.len() > 64
        || value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "image names must be 1-64 ASCII letters, digits, hyphens, or underscores".into(),
        );
    }
    Ok(value.into())
}

fn base_image_arn(region: &str, image: &str) -> String {
    if image.starts_with("arn:") {
        image.to_owned()
    } else {
        format!("arn:aws:lambda:{region}:aws:microvm-image:{image}")
    }
}
