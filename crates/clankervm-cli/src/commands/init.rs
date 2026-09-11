use crate::output::render;
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde::Serialize;
use std::fmt::Write as _;
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitResult {
    config_path: PathBuf,
}

pub(super) fn execute(
    args: &InitArgs,
    config_path: &Path,
    format: OutputFormat,
) -> Result<(), ClankerError> {
    if config_path.exists() && !args.force {
        return Err(ClankerError::AlreadyInitialized(config_path.to_owned()));
    }

    fs::write(config_path, template(args)).map_err(|source| ClankerError::Io {
        action: format!("write {}", config_path.display()),
        source,
    })?;

    let result = InitResult {
        config_path: config_path.to_owned(),
    };

    render(format, &result, || {
        format!("Initialized {}", config_path.display())
    })
}

fn template(args: &InitArgs) -> String {
    let mut text = format!(
        "[aws]\nregion = {}\n\n[microvm]\nname = {}\n\n[microvm.image]\n",
        toml_string(&args.region),
        toml_string(&args.name)
    );
    entry(
        &mut text,
        "iam-role",
        args.build_role_arn.as_deref(),
        "arn:aws:iam::123456789012:role/clankervm-build",
    );
    text.push_str("\n[microvm.image.artifact]\nsource = \".\"\n");
    entry(
        &mut text,
        "s3-bucket",
        args.artifact_bucket.as_deref(),
        "my-artifact-bucket",
    );
    text.push_str("\n[microvm.run]\n");
    entry(
        &mut text,
        "iam-role",
        args.execution_role_arn.as_deref(),
        "arn:aws:iam::123456789012:role/clankervm-run",
    );
    text
}

fn entry(text: &mut String, key: &str, value: Option<&str>, example: &str) {
    let (comment, value) = match value {
        Some(value) => ("", toml_string(value)),
        None => ("# ", toml_string(example)),
    };
    writeln!(text, "{comment}{key} = {value}").expect("writing to a String cannot fail");
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.into()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;
    use tempfile::TempDir;

    #[test]
    fn template_escapes_values_as_valid_toml() {
        let args = InitArgs {
            name: "quoted \"name\"\nwith control \u{7}".into(),
            region: "region\\name".into(),
            artifact_bucket: Some("bucket\nname".into()),
            build_role_arn: None,
            execution_role_arn: None,
            force: false,
        };

        let directory = TempDir::new().unwrap();
        let path = directory.path().join("clankervm.toml");
        fs::write(&path, template(&args)).unwrap();
        let config = ProjectConfig::load(&path, None).unwrap();

        assert_eq!(config.name, args.name);
        assert_eq!(config.aws.region, args.region);
        assert_eq!(config.push.artifact_bucket, args.artifact_bucket);
    }
}
