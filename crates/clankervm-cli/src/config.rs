use crate::commands::{PushConfig, RunConfig, StatusConfig};
use crate::util::validate_non_empty;
use crate::{ClankerError, Tags};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env::current_dir;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub schema_version: u32,
    pub app: AppConfig,
    #[serde(default)]
    pub push: PushConfig,
    #[serde(default)]
    pub status: StatusConfig,
    #[serde(default)]
    pub run: RunConfig,
    #[serde(default)]
    pub image: BTreeMap<String, ImageConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct ImageConfig {
    #[serde(flatten)]
    pub push: PushConfig,
    pub execution_role_arn: Option<String>,
    pub log_group: Option<String>,
    pub max_duration: Option<i32>,
    pub ingress: Option<String>,
    #[serde(rename = "run-egress")]
    pub run_egress: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AppConfig {
    pub name: String,
    pub region: String,
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
        validate_non_empty(Some(&config.app.name), "app.name")?;
        validate_non_empty(Some(&config.app.region), "app.region")?;
        validate_push(&config.push, "push")?;
        validate_run(&config.run, "run")?;
        for (name, image) in &config.image {
            if name.trim().is_empty() {
                return Err(ClankerError::InvalidConfig(
                    "image profile names must not be empty".into(),
                ));
            }
            let prefix = format!("image.{name}");
            validate_push(&image.push, &prefix)?;
            validate_non_empty(
                image.execution_role_arn.as_deref(),
                &format!("{prefix}.execution-role-arn"),
            )?;
            validate_non_empty(image.log_group.as_deref(), &format!("{prefix}.log-group"))?;
            validate_non_empty(image.ingress.as_deref(), &format!("{prefix}.ingress"))?;
            validate_non_empty(image.run_egress.as_deref(), &format!("{prefix}.run-egress"))?;
        }
        Ok(config)
    }

    pub fn select_image(
        &self,
        requested: Option<&str>,
        release_name: Option<&str>,
    ) -> Result<SelectedImage, ClankerError> {
        if let (Some(requested), Some(release_name)) = (requested, release_name)
            && requested != release_name
        {
            return Err(ClankerError::InvalidConfig(format!(
                "--image `{requested}` conflicts with release `{release_name}`"
            )));
        }

        let selected = requested.or(release_name);
        if self.image.is_empty() {
            return Ok(SelectedImage {
                name: selected.unwrap_or(&self.app.name).into(),
                region: self.app.region.clone(),
                push: self.push.clone(),
                run: self.run.clone(),
            });
        }

        let name = selected.ok_or_else(|| {
            ClankerError::InvalidConfig(
                "--image is required when image profiles are configured".into(),
            )
        })?;
        let profile = self.image.get(name).ok_or_else(|| {
            ClankerError::InvalidConfig(format!("unknown image profile `{name}`"))
        })?;
        let run = RunConfig {
            execution_role_arn: profile.execution_role_arn.clone(),
            log_group: profile.log_group.clone(),
            max_duration: profile.max_duration,
            ingress: profile.ingress.clone(),
            egress: profile.run_egress.clone(),
        }
        .overlay(&self.run);
        Ok(SelectedImage {
            name: name.into(),
            region: self.app.region.clone(),
            push: profile.push.clone().overlay(&self.push),
            run,
        })
    }
}

fn validate_push(config: &PushConfig, prefix: &str) -> Result<(), ClankerError> {
    validate_non_empty(
        config.artifact_bucket.as_deref(),
        &format!("{prefix}.artifact-bucket"),
    )?;
    validate_non_empty(
        config.build_role_arn.as_deref(),
        &format!("{prefix}.build-role-arn"),
    )?;
    validate_non_empty(
        config.base_image.as_deref(),
        &format!("{prefix}.base-image"),
    )?;
    validate_non_empty(config.egress.as_deref(), &format!("{prefix}.egress"))?;
    Tags::parse(config.tags.as_deref().unwrap_or_default()).map(|_| ())
}

fn validate_run(config: &RunConfig, prefix: &str) -> Result<(), ClankerError> {
    validate_non_empty(
        config.execution_role_arn.as_deref(),
        &format!("{prefix}.execution-role-arn"),
    )?;
    validate_non_empty(config.log_group.as_deref(), &format!("{prefix}.log-group"))?;
    validate_non_empty(config.ingress.as_deref(), &format!("{prefix}.ingress"))?;
    validate_non_empty(config.egress.as_deref(), &format!("{prefix}.egress"))
}

#[derive(Debug)]
pub(crate) struct SelectedImage {
    pub name: String,
    pub region: String,
    pub push: PushConfig,
    pub run: RunConfig,
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
            validate_non_empty(Some(&region), "app.region")?;
            config.app.region = region;
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
    use crate::test_support::ProjectConfigBuilder;

    const CONFIG: &str = r#"schema-version = 1
[app]
name = "demo"
region = "us-east-1"
[push]
capabilities = ["ALL"]
tags = ["team=platform"]
"#;

    #[test]
    fn capabilities_and_tags_use_flat_push_schema() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();
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
    fn image_profiles_inherit_root_settings_and_override_selected_values() {
        let config = ProjectConfigBuilder::new()
            .push(PushConfig {
                artifact_bucket: Some("root-bucket".into()),
                port: Some(8000),
                ..PushConfig::default()
            })
            .run(RunConfig {
                max_duration: Some(120),
                ..RunConfig::default()
            })
            .image(
                "agent",
                ImageConfig {
                    push: PushConfig {
                        context: Some("agent".into()),
                        artifact_bucket: Some("agent-bucket".into()),
                        ..PushConfig::default()
                    },
                    execution_role_arn: Some("arn:aws:iam::123456789012:role/agent".into()),
                    run_egress: Some("PRIVATE_EGRESS".into()),
                    ..ImageConfig::default()
                },
            )
            .image(
                "worker",
                ImageConfig {
                    push: PushConfig {
                        context: Some("worker".into()),
                        ..PushConfig::default()
                    },
                    ..ImageConfig::default()
                },
            )
            .build();
        let selected = config.select_image(Some("agent"), None).unwrap();
        assert_eq!(selected.name, "agent");
        assert_eq!(selected.region, "us-east-1");
        assert_eq!(selected.push.context.as_deref(), Some(Path::new("agent")));
        assert_eq!(selected.push.port, Some(8000));
        assert_eq!(
            selected.push.artifact_bucket.as_deref(),
            Some("agent-bucket")
        );
        assert_eq!(
            selected.run.execution_role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/agent")
        );
        assert_eq!(selected.run.max_duration, Some(120));
        assert_eq!(selected.run.egress.as_deref(), Some("PRIVATE_EGRESS"));
    }

    #[test]
    fn image_profile_selection_requires_a_name_and_rejects_conflicts() {
        let config = ProjectConfigBuilder::new()
            .image("agent", ImageConfig::default())
            .image("worker", ImageConfig::default())
            .build();
        assert!(config.select_image(None, None).is_err());
        assert!(config.select_image(Some("agent"), Some("worker")).is_err());
    }

    #[test]
    fn unknown_and_invocation_only_fields_are_rejected() {
        for text in [
            "schema-version = 1\n[app]\nname = 'x'\nregion = 'r'\n[wat]\nvalue = 1",
            "schema-version = 1\n[app]\nname = 'x'\nregion = 'r'\n[push]\nbundle = 'image.zip'",
        ] {
            let error = toml::from_str::<ProjectConfig>(text).unwrap_err();
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;

    #[test]
    fn root_of_bare_config_file_is_the_current_directory() {
        assert_eq!(project_root(Path::new("clankervm.toml")), Path::new("."));
        assert_eq!(
            project_root(Path::new("project/clankervm.toml")),
            Path::new("project")
        );
    }
}
