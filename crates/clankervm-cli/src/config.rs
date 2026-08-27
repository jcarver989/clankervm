use crate::client::ImageCapability;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ClankerError;

#[derive(Clone, Debug, Deserialize, Serialize)]
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AppConfig {
    pub name: String,
    pub region: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct PushConfig {
    pub context: PathBuf,
    pub artifact_bucket: Option<String>,
    pub build_role_arn: Option<String>,
    pub base_image: Option<String>,
    pub minimum_memory_mib: Option<i32>,
    #[serde(with = "capabilities")]
    pub capabilities: Vec<ImageCapability>,
    pub egress: Option<String>,
    pub keep_versions: Option<usize>,
    pub tags: Vec<String>,
    pub port: i32,
    pub ready_timeout_seconds: i32,
    pub run_timeout_seconds: i32,
    pub terminate_timeout_seconds: i32,
    #[serde(with = "human_duration")]
    pub timeout: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct StatusConfig {
    #[serde(with = "human_duration")]
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct RunConfig {
    pub execution_role_arn: Option<String>,
    pub log_group: Option<String>,
    pub max_duration: Option<i32>,
    pub ingress: Option<String>,
    pub egress: Option<String>,
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
        if config.app.name.is_empty() {
            return Err(ClankerError::InvalidConfig(
                "app.name must not be empty".into(),
            ));
        }
        if config.app.region.is_empty() {
            return Err(ClankerError::InvalidConfig(
                "app.region must not be empty".into(),
            ));
        }
        parse_tags(&config.push.tags)?;
        Ok(config)
    }
}

pub fn parse_tags(values: &[String]) -> Result<BTreeMap<String, String>, ClankerError> {
    let mut tags = BTreeMap::new();
    for value in values {
        let (key, tag_value) = value.split_once('=').ok_or_else(|| {
            ClankerError::InvalidConfig(format!("invalid tag `{value}`; expected key=value"))
        })?;
        if key.is_empty() || tag_value.is_empty() {
            return Err(ClankerError::InvalidConfig(format!(
                "invalid tag `{value}`; key and value must not be empty"
            )));
        }
        if tags.insert(key.to_owned(), tag_value.to_owned()).is_some() {
            return Err(ClankerError::InvalidConfig(format!(
                "duplicate tag key `{key}`"
            )));
        }
    }
    Ok(tags)
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            context: PathBuf::from("."),
            artifact_bucket: None,
            build_role_arn: None,
            base_image: None,
            minimum_memory_mib: None,
            capabilities: Vec::new(),
            egress: None,
            keep_versions: None,
            tags: Vec::new(),
            port: 9000,
            ready_timeout_seconds: 300,
            run_timeout_seconds: 60,
            terminate_timeout_seconds: 30,
            timeout: Duration::from_hours(1),
        }
    }
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_hours(1),
        }
    }
}

mod capabilities {
    use crate::client::ImageCapability;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        values: &[ImageCapability],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(ImageCapability::as_str)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<ImageCapability>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .into_iter()
            .map(|value| match value.as_str() {
                "ALL" => Ok(ImageCapability::All),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown image capability `{value}`"
                ))),
            })
            .collect()
    }
}

mod human_duration {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&humantime::format_duration(*value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        humantime::parse_duration(&String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(config.push.capabilities, [ImageCapability::All]);
        assert_eq!(parse_tags(&config.push.tags).unwrap()["team"], "platform");
        assert!(
            toml::to_string(&config)
                .unwrap()
                .contains("tags = [\"team=platform\"]")
        );
    }

    #[test]
    fn malformed_and_duplicate_tags_are_rejected() {
        for tags in [
            vec!["missing-equals".into()],
            vec!["=value".into()],
            vec!["key=".into()],
            vec!["key=a".into(), "key=b".into()],
        ] {
            assert!(parse_tags(&tags).is_err());
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = toml::from_str::<ProjectConfig>(
            "schema-version = 1\n[app]\nname = 'x'\nregion = 'r'\n[wat]\nvalue = 1",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
