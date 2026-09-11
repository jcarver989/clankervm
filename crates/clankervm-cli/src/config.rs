use crate::ClankerError;
use crate::arn::Arn;
use crate::client::{ImageConfiguration, ImageSpec};
use crate::commands::{PushSettings, RunSettings, StatusSettings};
use crate::util::{parse_release, validate_non_empty};
use aws_sdk_lambdamicrovms::types::{HookState, Hooks, MicrovmHooks, MicrovmImageHooks, Resources};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// The whole `clankervm.toml` file: one image and its command settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub schema_version: u32,
    pub image: ImageConfig,
    #[serde(default)]
    pub push: PushSettings,
    #[serde(default)]
    pub status: StatusSettings,
    #[serde(default)]
    pub run: RunSettings,
    #[serde(skip)]
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ImageConfig {
    pub name: String,
    pub region: String,
    pub profile: Option<String>,
}

impl ProjectConfig {
    pub(crate) fn load(path: &Path, region: Option<String>) -> Result<Self, ClankerError> {
        let text = fs::read_to_string(path).map_err(|source| ClankerError::ConfigIo {
            path: path.to_owned(),
            source,
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|source| ClankerError::Config {
            path: path.to_owned(),
            source,
        })?;
        if config.schema_version != 1 {
            return Err(ClankerError::InvalidConfig(format!(
                "unsupported schema-version {}; expected 1",
                config.schema_version
            )));
        }
        if let Some(region) = region {
            config.image.region = region;
        }
        config.root = path.parent().map_or_else(PathBuf::new, Path::to_owned);
        validate_non_empty(Some(&config.image.name), "image.name")?;
        validate_non_empty(Some(&config.image.region), "image.region")?;
        validate_non_empty(config.image.profile.as_deref(), "image.profile")?;
        config.push.validate()?;
        config.run.validate()?;
        config.status.validate()?;
        Ok(config)
    }

    pub(crate) fn resolve(&self, path: &Path) -> PathBuf {
        self.root.join(path)
    }

    pub(crate) fn image_arn(&self, role: &Arn) -> Result<Arn, ClankerError> {
        let account = role
            .account()
            .ok_or_else(|| ClankerError::InvalidConfig(format!("invalid IAM role ARN `{role}`")))?;
        Arn::lambda(
            &self.image.region,
            account,
            "microvm-image",
            &self.image.name,
        )
    }

    pub(crate) fn target(
        &self,
        release: Option<&str>,
        account_role: &str,
    ) -> Result<Target, ClankerError> {
        Ok(Target {
            name: self.image.name.clone(),
            region: self.image.region.clone(),
            arn: self.image_arn(&Arn::parse(account_role)?)?,
            version: self.version(release)?,
        })
    }

    pub(crate) fn account_role<'a>(
        &'a self,
        run: &'a RunSettings,
    ) -> Result<&'a str, ClankerError> {
        self.push
            .build_role_arn
            .as_deref()
            .or(run.execution_role_arn.as_deref())
            .ok_or_else(|| {
                ClankerError::InvalidConfig(
                    "push.build-role-arn or run.execution-role-arn must be configured".into(),
                )
            })
    }

    pub(crate) fn image_spec(
        &self,
        settings: &PushSettings,
        bundle_digest: &str,
    ) -> Result<ImageSpec, ClankerError> {
        let region = &self.image.region;
        let role = Arn::parse(settings.build_role_arn()?)?;
        Ok(ImageSpec {
            arn: self.image_arn(&role)?,
            name: self.image.name.clone(),
            bucket: settings.artifact_bucket()?.to_owned(),
            tags: settings.tags()?,
            configuration: ImageConfiguration {
                base_image_arn: Arn::lambda(region, "aws", "microvm-image", settings.base_image())?,
                build_role_arn: role,
                description: format!("Bundle {bundle_digest}"),
                resources: image_resources(settings)?,
                capabilities: settings.capabilities()?,
                egress_network_connector: Arn::network_connector(region, settings.egress())?,
                hooks: image_hooks(settings),
            },
        })
    }

    fn version(&self, release: Option<&str>) -> Result<Option<String>, ClankerError> {
        let Some(release) = release else {
            return Ok(None);
        };
        let (name, version) = parse_release(release)?;
        if name != self.image.name {
            return Err(ClankerError::InvalidConfig(format!(
                "release `{release}` does not match configured image `{}`",
                self.image.name
            )));
        }
        Ok(Some(version.to_owned()))
    }
}

/// The hook configuration AWS accepts on both create and update.
fn image_hooks(settings: &PushSettings) -> Hooks {
    Hooks::builder()
        .port(settings.port())
        .microvm_image_hooks(
            MicrovmImageHooks::builder()
                .ready(HookState::Enabled)
                .ready_timeout_in_seconds(settings.ready_timeout_seconds())
                .build(),
        )
        .microvm_hooks(
            MicrovmHooks::builder()
                .run(HookState::Enabled)
                .run_timeout_in_seconds(settings.run_timeout_seconds())
                .terminate(HookState::Enabled)
                .terminate_timeout_in_seconds(settings.terminate_timeout_seconds())
                .build(),
        )
        .build()
}

/// The memory request AWS accepts on both create and update.
fn image_resources(settings: &PushSettings) -> Result<Option<Vec<Resources>>, ClankerError> {
    settings
        .minimum_memory_mib
        .map(|memory| Resources::builder().minimum_memory_in_mib(memory).build())
        .transpose()
        .map(|resources| resources.map(|resources| vec![resources]))
        .map_err(|error| {
            ClankerError::InvalidConfig(format!("invalid push.minimum-memory-mib: {error}"))
        })
}

pub(crate) trait Settings: Serialize + DeserializeOwned {
    fn validate(&self) -> Result<(), ClankerError> {
        Ok(())
    }

    fn merge(&self, higher: &Self) -> Result<Self, ClankerError> {
        let mut merged = to_value(self)?;
        overlay(&mut merged, to_value(higher)?);
        let merged: Self = serde_json::from_value(merged).map_err(|error| {
            ClankerError::InvalidConfig(format!("invalid configuration: {error}"))
        })?;
        merged.validate()?;
        Ok(merged)
    }
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, ClankerError> {
    serde_json::to_value(value)
        .map_err(|error| ClankerError::InvalidConfig(format!("invalid configuration: {error}")))
}

fn overlay(base: &mut Value, higher: Value) {
    let Value::Object(higher) = higher else {
        *base = higher;
        return;
    };
    let Value::Object(base) = base else {
        *base = Value::Object(higher);
        return;
    };
    for (key, value) in higher {
        if value.is_null() {
            continue;
        }
        match base.get_mut(&key) {
            Some(slot) => overlay(slot, value),
            None => {
                base.insert(key, value);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Target {
    pub name: String,
    pub region: String,
    pub arn: Arn,
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"schema-version = 1
[image]
name = "demo"
region = "us-east-1"
profile = "Production-PowerUser"
[push]
capabilities = ["ALL"]
tags = ["team=platform"]
[run]
execution-role-arn = "arn:aws:iam::123456789012:role/run"
"#;

    #[test]
    fn merge_keeps_lower_values_and_lets_higher_override() {
        let lower = PushSettings {
            context: Some("root".into()),
            port: Some(8000),
            artifact_bucket: Some("bucket".into()),
            ..PushSettings::default()
        };
        let higher = PushSettings {
            context: Some("cli".into()),
            ..PushSettings::default()
        };

        let merged = lower.merge(&higher).unwrap();

        assert_eq!(merged.context, Some(PathBuf::from("cli")));
        assert_eq!(merged.port, Some(8000));
        assert_eq!(merged.artifact_bucket.as_deref(), Some("bucket"));
    }

    #[test]
    fn merge_replaces_lists_instead_of_extending_them() {
        let lower = PushSettings {
            tags: Some(vec!["team=platform".into()]),
            ..PushSettings::default()
        };
        let higher = PushSettings {
            tags: Some(vec!["owner=cli".into()]),
            ..PushSettings::default()
        };

        let merged = lower.merge(&higher).unwrap();

        assert_eq!(merged.tags, Some(vec!["owner=cli".to_string()]));
    }

    #[test]
    fn capabilities_and_tags_use_flat_push_schema() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();
        assert_eq!(
            config.image.profile.as_deref(),
            Some("Production-PowerUser")
        );
        assert_eq!(
            config.push.capabilities.as_deref(),
            Some(&["ALL".to_owned()][..])
        );
        assert_eq!(
            config.push.tags().unwrap().get("team").map(String::as_str),
            Some("platform")
        );
    }

    #[test]
    fn image_arn_uses_the_account_of_the_role() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();
        let role = Arn::parse("arn:aws:iam::123456789012:role/run").unwrap();

        assert_eq!(
            config.image_arn(&role).unwrap().as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );

        let malformed = Arn::parse("arn:aws:iam:::role").unwrap();
        assert!(matches!(
            config.image_arn(&malformed),
            Err(ClankerError::InvalidConfig(_))
        ));
    }

    #[test]
    fn explicit_release_must_match_the_configured_image() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();

        let target = config
            .target(Some("demo@7"), "arn:aws:iam::123456789012:role/run")
            .unwrap();
        assert_eq!(target.name, "demo");
        assert_eq!(target.version.as_deref(), Some("7"));

        let error = config
            .target(Some("other@7"), "arn:aws:iam::123456789012:role/run")
            .unwrap_err();
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
    fn paths_resolve_against_the_project_file_directory() {
        let directory = tempfile::TempDir::new().unwrap();
        let config = crate::test_support::project(directory.path(), "");

        assert_eq!(
            config.resolve(Path::new("app")),
            directory.path().join("app")
        );
        assert_eq!(config.resolve(Path::new("/abs")), Path::new("/abs"));
    }
}
