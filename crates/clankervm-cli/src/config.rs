use crate::ClankerError;
use crate::arn::Arn;
use crate::client::{ImageConfiguration, ImageSpec};
use crate::commands::{LogsSettings, PushSettings, RunSettings, StatusSettings};
use crate::payload::build_ready_payload;
use crate::util::{parse_key_values, parse_release, validate_non_empty};
use aws_sdk_lambdamicrovms::types::{HookState, Hooks, MicrovmHooks, MicrovmImageHooks, Resources};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Project configuration resolved into command settings, before CLI overrides.
#[derive(Debug, Deserialize)]
#[serde(try_from = "ProjectFile")]
pub struct ProjectConfig {
    pub aws: AwsConfig,
    pub name: String,
    pub push: PushSettings,
    pub status: StatusSettings,
    pub run: RunSettings,
    pub logs: LogsSettings,
    root: PathBuf,
    ready: Option<ReadyHookConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    pub region: String,
    pub profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    aws: AwsConfig,
    microvm: MicrovmConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MicrovmConfig {
    name: String,
    #[serde(default)]
    image: ImageConfig,
    #[serde(default)]
    run: RunConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
struct ImageConfig {
    base_image: Option<String>,
    minimum_memory_mib: Option<i32>,
    os_capabilities: Option<Vec<String>>,
    iam_role: Option<String>,
    tags: Option<Vec<String>>,
    artifact: ArtifactConfig,
    network: ImageNetworkConfig,
    hooks: HooksConfig,
    versions: VersionsConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
struct ArtifactConfig {
    source: Option<PathBuf>,
    s3_bucket: Option<String>,
    s3_prefix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ImageNetworkConfig {
    egress: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
struct HooksConfig {
    port: Option<i32>,
    ready: Option<ReadyHookConfig>,
    #[serde(with = "humantime_serde")]
    ready_timeout: Option<Duration>,
    #[serde(with = "humantime_serde")]
    run_timeout: Option<Duration>,
    #[serde(with = "humantime_serde")]
    terminate_timeout: Option<Duration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyHookConfig {
    command: Vec<String>,
    #[serde(default)]
    environment: Vec<String>,
}

impl ReadyHookConfig {
    fn payload(&self, region: &str) -> Result<String, ClankerError> {
        let (command, args) = self.command.split_first().ok_or_else(|| {
            ClankerError::InvalidConfig(
                "microvm.image.hooks.ready.command must not be empty".into(),
            )
        })?;
        let environment = parse_key_values(&self.environment, "ready environment variable", false)?;
        build_ready_payload(command, args, environment, region)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
struct VersionsConfig {
    max: Option<usize>,
    #[serde(with = "humantime_serde")]
    wait_timeout: Option<Duration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
struct RunConfig {
    iam_role: Option<String>,
    command: Option<Vec<String>>,
    environment: Option<Vec<String>>,
    max_duration: Option<i32>,
    network: RunNetworkConfig,
    logs: LogsSettings,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RunNetworkConfig {
    ingress: Option<String>,
    egress: Option<String>,
}

impl TryFrom<ProjectFile> for ProjectConfig {
    type Error = ClankerError;

    fn try_from(file: ProjectFile) -> Result<Self, Self::Error> {
        let image = file.microvm.image;
        let run = file.microvm.run;
        if let Some(ready) = &image.hooks.ready {
            ready.payload(&file.aws.region)?;
        }
        Ok(Self {
            aws: file.aws,
            name: file.microvm.name,
            push: PushSettings {
                context: image.artifact.source,
                artifact_bucket: image.artifact.s3_bucket,
                artifact_prefix: image.artifact.s3_prefix,
                build_role_arn: image.iam_role,
                base_image: image.base_image,
                minimum_memory_mib: image.minimum_memory_mib,
                capabilities: image.os_capabilities,
                egress: image.network.egress,
                keep_versions: image.versions.max,
                tags: image.tags,
                port: image.hooks.port,
                ready_timeout_seconds: hook_seconds(image.hooks.ready_timeout, "ready-timeout")?,
                run_timeout_seconds: hook_seconds(image.hooks.run_timeout, "run-timeout")?,
                terminate_timeout_seconds: hook_seconds(
                    image.hooks.terminate_timeout,
                    "terminate-timeout",
                )?,
                timeout: image.versions.wait_timeout,
            },
            status: StatusSettings {
                timeout: image.versions.wait_timeout,
            },
            run: RunSettings {
                command: run.command,
                execution_role_arn: run.iam_role,
                ingress: run.network.ingress,
                egress: run.network.egress,
                max_duration: run.max_duration,
                environment: run.environment,
                log_group: run.logs.log_group.clone(),
            },
            logs: run.logs,
            root: PathBuf::new(),
            ready: image.hooks.ready,
        })
    }
}

fn hook_seconds(duration: Option<Duration>, field: &str) -> Result<Option<i32>, ClankerError> {
    duration.map(|duration| {
        let invalid = || ClankerError::InvalidConfig(format!(
            "microvm.image.hooks.{field} must be a whole number of seconds no greater than {}",
            i32::MAX
        ));
        if duration.subsec_nanos() != 0 {
            return Err(invalid());
        }
        i32::try_from(duration.as_secs()).map_err(|_| invalid())
    }).transpose()
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
        if let Some(region) = region {
            config.aws.region = region;
        }
        config.root = path.parent().map_or_else(PathBuf::new, Path::to_owned);
        validate_non_empty(Some(&config.name), "microvm.name")?;
        validate_non_empty(Some(&config.aws.region), "aws.region")?;
        validate_non_empty(config.aws.profile.as_deref(), "aws.profile")?;
        config.push.validate()?;
        config.run.validate()?;
        config.status.validate()?;
        config.logs.validate()?;
        Ok(config)
    }

    pub(crate) fn resolve(&self, path: &Path) -> PathBuf {
        self.root.join(path)
    }

    pub(crate) fn image_arn(&self, role: &Arn) -> Result<Arn, ClankerError> {
        let account = role
            .account()
            .ok_or_else(|| ClankerError::InvalidConfig(format!("invalid IAM role ARN `{role}`")))?;
        Arn::lambda(&self.aws.region, account, "microvm-image", &self.name)
    }

    pub(crate) fn target(
        &self,
        release: Option<&str>,
        account_role: &str,
    ) -> Result<Target, ClankerError> {
        Ok(Target {
            name: self.name.clone(),
            region: self.aws.region.clone(),
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
                    "microvm.image.iam-role or microvm.run.iam-role must be configured".into(),
                )
            })
    }

    pub(crate) fn image_spec(
        &self,
        settings: &PushSettings,
        bundle_digest: &str,
    ) -> Result<ImageSpec, ClankerError> {
        let region = &self.aws.region;
        let role = Arn::parse(settings.build_role_arn()?)?;
        Ok(ImageSpec {
            arn: self.image_arn(&role)?,
            name: self.name.clone(),
            bucket: settings.artifact_bucket()?.to_owned(),
            artifact_prefix: settings.artifact_prefix().to_owned(),
            tags: settings.tags()?,
            configuration: ImageConfiguration {
                base_image_arn: Arn::lambda(region, "aws", "microvm-image", settings.base_image())?,
                build_role_arn: role,
                description: format!("Bundle {bundle_digest}"),
                resources: image_resources(settings)?,
                capabilities: settings.capabilities()?,
                egress_network_connector: Arn::network_connector(region, settings.egress())?,
                hooks: image_hooks(settings),
                environment_variables: self.image_environment()?,
            },
        })
    }

    fn image_environment(&self) -> Result<HashMap<String, String>, ClankerError> {
        let mut environment = HashMap::new();
        if let Some(ready) = &self.ready {
            environment.insert(
                "CLANKERVM_READY_HOOK_PAYLOAD".into(),
                ready.payload(&self.aws.region)?,
            );
        }
        Ok(environment)
    }

    fn version(&self, release: Option<&str>) -> Result<Option<String>, ClankerError> {
        let Some(release) = release else {
            return Ok(None);
        };
        let (name, version) = parse_release(release)?;
        if name != self.name {
            return Err(ClankerError::InvalidConfig(format!(
                "release `{release}` does not match configured image `{}`",
                self.name
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
            ClankerError::InvalidConfig(format!(
                "invalid microvm.image.minimum-memory-mib: {error}"
            ))
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

    const CONFIG: &str = r#"[aws]
region = "us-east-1"
profile = "Production-PowerUser"
[microvm]
name = "demo"
[microvm.image]
os-capabilities = ["ALL"]
tags = ["team=platform"]
[microvm.run]
iam-role = "arn:aws:iam::123456789012:role/run"
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
    fn ready_command_uses_run_shaped_settings_and_the_resolved_region() {
        let text = format!(
            "{CONFIG}\n[microvm.image.hooks.ready]\ncommand = ['echo', 'hello world']\nenvironment = ['EMPTY=', 'VALUE=a=b', 'AWS_REGION=ignored']"
        );
        let mut config: ProjectConfig = toml::from_str(&text).unwrap();
        config.aws.region = "us-west-2".into();
        let environment = config.image_environment().unwrap();
        let payload: Value =
            serde_json::from_str(&environment["CLANKERVM_READY_HOOK_PAYLOAD"]).unwrap();
        assert_eq!(payload["command"], "echo");
        assert_eq!(payload["args"], serde_json::json!(["hello world"]));
        assert_eq!(payload["environment"]["EMPTY"], "");
        assert_eq!(payload["environment"]["VALUE"], "a=b");
        assert_eq!(payload["environment"]["AWS_REGION"], "us-west-2");
        assert_eq!(payload["environment"]["AWS_DEFAULT_REGION"], "us-west-2");
        let default: ProjectConfig = toml::from_str(CONFIG).unwrap();
        assert!(default.image_environment().unwrap().is_empty());
    }

    #[test]
    fn ready_command_rejects_invalid_or_oversized_configuration() {
        for fields in [
            "environment = ['KEY=value']".to_owned(),
            "command = []".into(),
            "command = [' ']".into(),
            "command = ['echo']\nenvironment = ['missing-equals']".into(),
            "command = ['echo']\nenvironment = ['=value']".into(),
            "command = ['echo']\nunknown = true".into(),
            "command = [\"echo\\u0000\"]".into(),
            "command = ['echo', \"bad\\u0000arg\"]".into(),
            "command = ['echo']\nenvironment = [\"KEY=bad\\u0000value\"]".into(),
            format!("command = ['echo', '{}']", "x".repeat(4096)),
        ] {
            let text = format!("{CONFIG}\n[microvm.image.hooks.ready]\n{fields}");
            assert!(
                toml::from_str::<ProjectConfig>(&text).is_err(),
                "accepted {fields}"
            );
        }
    }

    #[test]
    fn capabilities_and_tags_use_image_schema() {
        let config: ProjectConfig = toml::from_str(CONFIG).unwrap();
        assert_eq!(config.aws.profile.as_deref(), Some("Production-PowerUser"));
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
    fn old_schemas_are_rejected() {
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
    fn documented_config_resolves_every_nested_setting() {
        let documentation = include_str!("../README.md");
        let text = documentation
            .split("```toml\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let config: ProjectConfig = toml::from_str(text).unwrap();
        assert_eq!(config.aws.region, "us-west-2");
        assert_eq!(config.aws.profile.as_deref(), Some("my-aws-profile"));
        assert_eq!(config.name, "my-runner");
        let push = &config.push;
        assert_eq!(push.context.as_deref(), Some(Path::new(".")));
        assert_eq!(push.artifact_bucket.as_deref(), Some("my-artifact-bucket"));
        assert_eq!(push.artifact_prefix.as_deref(), Some("clankervm"));
        assert_eq!(push.base_image.as_deref(), Some("al2023-1"));
        assert_eq!(push.minimum_memory_mib, Some(512));
        assert_eq!(push.capabilities.as_deref(), Some(&["ALL".to_owned()][..]));
        assert_eq!(
            push.tags.as_deref(),
            Some(&["team=platform".to_owned()][..])
        );
        assert_eq!(
            push.build_role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/clankervm-build")
        );
        assert_eq!(push.egress.as_deref(), Some("INTERNET_EGRESS"));
        assert_eq!(push.port, Some(9000));
        assert_eq!(push.ready_timeout_seconds, Some(300));
        assert_eq!(push.run_timeout_seconds, Some(60));
        assert_eq!(push.terminate_timeout_seconds, Some(30));
        assert_eq!(push.keep_versions, Some(10));
        assert_eq!(push.timeout, Some(Duration::from_secs(3600)));
        assert_eq!(config.status.timeout, push.timeout);
        assert_eq!(
            config.run.execution_role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/clankervm-execution")
        );
        assert_eq!(
            config.run.command.as_ref().unwrap(),
            &["/usr/local/bin/my-job", "--job-id", "42"]
        );
        assert_eq!(
            config.run.environment.as_deref(),
            Some(&["LOG_LEVEL=info".to_owned()][..])
        );
        assert_eq!(config.run.max_duration, Some(3600));
        assert_eq!(config.run.ingress.as_deref(), Some("NO_INGRESS"));
        assert_eq!(config.run.egress.as_deref(), Some("INTERNET_EGRESS"));
        assert_eq!(config.run.log_group.as_deref(), Some("/my-runner/microvms"));
        assert_eq!(config.logs.log_group, config.run.log_group);
        assert_eq!(config.logs.log_stream.as_deref(), Some("..."));
        assert_eq!(config.logs.since, Some(Duration::from_mins(30)));
        assert_eq!(config.logs.limit, Some(1000));
        assert_eq!(config.logs.timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn nested_tables_reject_unknown_and_misplaced_fields() {
        for section in [
            "[aws.extra]\nvalue = 1",
            "[microvm.image]\ncapabilities = ['ALL']",
            "[microvm.image.artifact]\ncontext = '.'",
            "[microvm.image.network]\ningress = 'NO_INGRESS'",
            "[microvm.image.hooks]\nready-timeout-seconds = 300",
            "[microvm.image.versions]\nkeep-versions = 10",
            "[microvm.run.network]\nunknown = true",
            "[microvm.run.logs]\nlog-group = '/logs'",
        ] {
            let text = format!("[aws]\nregion = 'us-east-1'\n[microvm]\nname = 'demo'\n{section}");
            assert!(
                toml::from_str::<ProjectConfig>(&text).is_err(),
                "accepted {section}"
            );
        }
    }

    #[test]
    fn hook_durations_must_be_representable_as_integer_seconds() {
        for field in ["ready-timeout", "run-timeout", "terminate-timeout"] {
            for value in ["'500ms'", "'2147483648s'", "'invalid'", "300", "'-1s'"] {
                let text = format!("{CONFIG}\n[microvm.image.hooks]\n{field} = {value}");
                assert!(
                    toml::from_str::<ProjectConfig>(&text).is_err(),
                    "accepted {field} = {value}"
                );
            }
        }
    }

    #[test]
    fn region_override_and_optional_tables_preserve_defaults() {
        let directory = tempfile::TempDir::new().unwrap();
        crate::test_support::project(directory.path(), "");
        let config = ProjectConfig::load(
            &directory.path().join("clankervm.toml"),
            Some("us-west-2".into()),
        )
        .unwrap();
        assert_eq!(config.aws.region, "us-west-2");
        assert!(config.aws.profile.is_none());
        assert_eq!(config.push.ready_timeout_seconds(), 300);
        assert_eq!(config.push.timeout(), Duration::from_secs(3600));
        assert_eq!(config.run.max_duration(), 3600);
        assert_eq!(config.logs.limit(), 1000);
        assert!(
            config
                .image_arn(&Arn::parse("arn:aws:iam::123456789012:role/run").unwrap())
                .unwrap()
                .as_str()
                .contains(":us-west-2:")
        );
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
