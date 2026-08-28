use crate::commands::{PushConfig, RunConfig, StatusConfig};
use crate::environment::RunEnvironment;
use crate::util::{parse_release, validate_non_empty};
use crate::{Arn, ClankerError, Tags};
use serde::Deserialize;
use std::env::current_dir;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub schema_version: u32,
    pub image: ImageConfig,
    #[serde(default)]
    pub push: PushConfig,
    #[serde(default)]
    pub status: StatusConfig,
    #[serde(default)]
    pub run: RunConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ImageConfig {
    pub name: String,
    pub region: String,
    pub profile: Option<String>,
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self, ClankerError> {
        let text = fs::read_to_string(path).map_err(|source| ClankerError::ConfigIo {
            path: path.to_owned(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ClankerError::Config {
            path: path.to_owned(),
            source,
        })?;
        if config.schema_version != 1 {
            return Err(ClankerError::InvalidConfig(format!(
                "unsupported schema-version {}; expected 1",
                config.schema_version
            )));
        }
        validate_non_empty(Some(&config.image.name), "image.name")?;
        validate_non_empty(Some(&config.image.region), "image.region")?;
        validate_non_empty(config.image.profile.as_deref(), "image.profile")?;
        validate_push(&config.push)?;
        validate_run(&config.run)?;
        Ok(config)
    }

    pub(crate) fn resolve_image(
        &self,
        release: Option<&str>,
    ) -> Result<ResolvedImage, ClankerError> {
        let version = if let Some(release) = release {
            let (name, version) = parse_release(release)?;
            if name != self.image.name {
                return Err(ClankerError::InvalidConfig(format!(
                    "release `{release}` does not match configured image `{}`",
                    self.image.name
                )));
            }
            Some(version.to_owned())
        } else {
            None
        };

        Ok(ResolvedImage {
            name: self.image.name.clone(),
            region: self.image.region.clone(),
            push: self.push.clone(),
            run: self.run.clone(),
            version,
        })
    }
}

fn validate_push(config: &PushConfig) -> Result<(), ClankerError> {
    validate_non_empty(config.artifact_bucket.as_deref(), "push.artifact-bucket")?;
    validate_non_empty(config.build_role_arn.as_deref(), "push.build-role-arn")?;
    validate_non_empty(config.base_image.as_deref(), "push.base-image")?;
    validate_non_empty(config.egress.as_deref(), "push.egress")?;
    Tags::parse(config.tags.as_deref().unwrap_or_default()).map(|_| ())
}

fn validate_run(config: &RunConfig) -> Result<(), ClankerError> {
    if config.command.as_deref().is_some_and(|command| {
        command
            .first()
            .is_none_or(|executable| executable.trim().is_empty())
    }) {
        return Err(ClankerError::InvalidConfig(
            "run.command must contain a non-empty executable".into(),
        ));
    }
    validate_non_empty(
        config.execution_role_arn.as_deref(),
        "run.execution-role-arn",
    )?;
    validate_non_empty(config.log_group.as_deref(), "run.log-group")?;
    RunEnvironment::parse(config.environment.as_deref().unwrap_or_default())?;
    validate_non_empty(config.ingress.as_deref(), "run.ingress")?;
    validate_non_empty(config.egress.as_deref(), "run.egress")
}

#[derive(Debug)]
pub(crate) struct ResolvedImage {
    pub name: String,
    pub region: String,
    pub push: PushConfig,
    pub run: RunConfig,
    pub version: Option<String>,
}

impl ResolvedImage {
    pub(crate) fn target(&self, account_role: &str) -> Result<ImageTarget, ClankerError> {
        let account = account_role
            .split(':')
            .nth(4)
            .filter(|account| !account.is_empty())
            .ok_or_else(|| {
                ClankerError::InvalidConfig(format!("invalid IAM role ARN `{account_role}`"))
            })?;
        Ok(ImageTarget {
            name: self.name.clone(),
            version: self.version.clone(),
            image_arn: Arn::image(&self.name, &self.region, account)?,
        })
    }

    pub(crate) fn configured_account_role(&self) -> Result<&str, ClankerError> {
        self.push
            .build_role_arn
            .as_deref()
            .or(self.run.execution_role_arn.as_deref())
            .ok_or_else(|| {
                ClankerError::InvalidConfig(
                    "push.build-role-arn or run.execution-role-arn must be configured".into(),
                )
            })
    }
}

pub(crate) struct ImageTarget {
    pub name: String,
    pub version: Option<String>,
    pub image_arn: Arn,
}

#[derive(Debug)]
pub struct Project {
    pub config: ProjectConfig,
    root: PathBuf,
}

impl Project {
    pub fn load(config_path: &Path, region: Option<String>) -> Result<Self, ClankerError> {
        let config_path = if config_path.is_absolute() {
            config_path.to_owned()
        } else {
            current_dir()
                .map_err(|source| ClankerError::Io {
                    action: "resolve current directory".into(),
                    source,
                })?
                .join(config_path)
        };
        let mut config = ProjectConfig::load(&config_path)?;
        if let Some(region) = region {
            validate_non_empty(Some(&region), "image.region")?;
            config.image.region = region;
        }
        let root = project_root(&config_path).to_owned();
        Ok(Self { config, root })
    }

    #[cfg(test)]
    pub(crate) fn from_parts(config: ProjectConfig, root: PathBuf) -> Self {
        Self { config, root }
    }

    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        }
    }
}

pub(crate) fn project_root(config_path: &Path) -> &Path {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ImageCapability;

    const CONFIG: &str = r#"schema-version = 1
[image]
name = "demo"
region = "us-east-1"
profile = "Production-PowerUser"
[push]
capabilities = ["ALL"]
tags = ["team=platform"]
"#;

    #[test]
    fn capabilities_and_tags_use_flat_push_schema() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();
        assert_eq!(
            config.image.profile.as_deref(),
            Some("Production-PowerUser")
        );
        assert_eq!(
            config.push.capabilities.as_deref(),
            Some(&[ImageCapability::All][..])
        );
        assert_eq!(
            Tags::parse(config.push.tags.as_deref().unwrap())
                .unwrap()
                .into_inner()
                .get("team")
                .map(String::as_str),
            Some("platform")
        );
    }

    #[test]
    fn explicit_release_must_match_the_configured_image() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();

        let resolved = config.resolve_image(Some("demo@7")).unwrap();
        assert_eq!(resolved.name, "demo");
        assert_eq!(resolved.version.as_deref(), Some("7"));

        let error = config.resolve_image(Some("other@7")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match configured image")
        );
    }

    #[test]
    fn old_app_and_nested_image_schemas_are_rejected() {
        for text in [
            "schema-version = 1\n[app]\nname = 'x'\nregion = 'r'",
            "schema-version = 1\n[image]\nname = 'x'\nregion = 'r'\n[image.agent]\ncontext = '.'",
            "schema-version = 1\n[image]\nname = 'x'\nregion = 'r'\n[push]\nbundle = 'image.zip'",
        ] {
            let error = toml::from_str::<ProjectConfig>(text).unwrap_err();
            assert!(
                error.to_string().contains("unknown field")
                    || error.to_string().contains("missing field"),
                "{error}"
            );
        }
    }

    #[test]
    fn root_of_bare_config_file_is_the_current_directory() {
        assert_eq!(project_root(Path::new("clankervm.toml")), Path::new("."));
        assert_eq!(
            project_root(Path::new("project/clankervm.toml")),
            Path::new("project")
        );
    }
}
