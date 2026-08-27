use super::render;
use crate::config::{AppConfig, ProjectConfig, PushConfig, RunConfig, StatusConfig};
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long, default_value = "us-west-2")]
    pub region: String,
    #[arg(long)]
    pub artifact_bucket: Option<String>,
    #[arg(long)]
    pub build_role_arn: Option<String>,
    #[arg(long)]
    pub execution_role_arn: Option<String>,
    #[arg(long)]
    pub force: bool,
}

pub(super) fn execute(
    args: &InitArgs,
    config_path: &Path,
    format: OutputFormat,
) -> Result<(), ClankerError> {
    init(args, config_path)?;
    let result = InitResult {
        config_path: config_path.to_owned(),
    };
    render(format, &result, || {
        format!("Initialized {}", config_path.display())
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitResult {
    config_path: PathBuf,
}

fn init(args: &InitArgs, path: &Path) -> Result<(), ClankerError> {
    if path.exists() && !args.force {
        return Err(ClankerError::AlreadyInitialized(path.to_owned()));
    }
    let mut config = ProjectConfig {
        schema_version: 1,
        app: AppConfig {
            name: args.name.clone(),
            region: args.region.clone(),
        },
        push: PushConfig::default(),
        status: StatusConfig::default(),
        run: RunConfig::default(),
    };
    config.push.artifact_bucket = Some(args.artifact_bucket.clone().unwrap_or_default());
    config.push.build_role_arn = Some(args.build_role_arn.clone().unwrap_or_default());
    config.run.execution_role_arn = Some(args.execution_role_arn.clone().unwrap_or_default());
    let text = toml::to_string_pretty(&config)
        .map_err(|error| ClankerError::InvalidConfig(error.to_string()))?;
    fs::write(path, text).map_err(|source| ClankerError::Io {
        action: format!("write {}", path.display()),
        source,
    })
}
