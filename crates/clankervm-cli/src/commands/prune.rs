use crate::arn::Arn;
use crate::client::{MicroVmClient, PruneReport};
use crate::config::{ProjectConfig, Settings};
use crate::output::render;
use crate::util::validate_keep_versions;
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Args)]
pub struct PruneOptions {
    #[command(flatten)]
    pub settings: PruneSettings,
}

impl PruneOptions {
    pub(super) fn keep_versions(&self, config: &ProjectConfig) -> Result<usize, ClankerError> {
        config
            .prune
            .merge(&self.settings)?
            .keep_versions
            .ok_or_else(|| {
                ClankerError::InvalidConfig(
                    "microvm.image.versions.max or --keep-versions must be configured".into(),
                )
            })
    }
}

/// Prune settings resolved from image versions and CLI flags.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct PruneSettings {
    #[arg(long)]
    pub keep_versions: Option<usize>,
}

impl Settings for PruneSettings {
    fn validate(&self) -> Result<(), ClankerError> {
        validate_keep_versions(self.keep_versions)
    }
}

/// What one prune did to the project's image.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PruneResult {
    image_name: String,
    image_arn: Arn,
    keep_versions: usize,
    #[serde(flatten)]
    report: PruneReport,
}

impl PruneResult {
    fn human(&self) -> String {
        let report = &self.report;
        let mut rows = vec![
            ("Deleted:", &report.deleted),
            ("Kept:", &report.kept),
            ("In use:", &report.in_use),
        ];
        if !report.deactivated.is_empty() {
            rows.push(("Left inactive:", &report.deactivated));
        }
        let mut lines = vec![format!("✓ Pruned {}", self.image_name)];
        for (label, versions) in rows {
            let versions = if versions.is_empty() {
                "none".to_owned()
            } else {
                versions.join(", ")
            };
            lines.push(format!("{label:<14} {versions}"));
        }
        lines.join("\n")
    }
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &PruneOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let result = prune(options, config, client).await?;
    render(format, &result, || result.human())
}

async fn prune<T: MicroVmClient>(
    options: &PruneOptions,
    config: &ProjectConfig,
    client: &T,
) -> Result<PruneResult, ClankerError> {
    let keep_versions = options.keep_versions(config)?;
    let target = config.target(None, config.account_role(&config.run)?)?;
    let report = client
        .prune(&target.arn, keep_versions)
        .await?
        .ok_or_else(|| ClankerError::ImageNotFound(target.arn.to_string()))?;
    Ok(PruneResult {
        image_name: target.name,
        image_arn: target.arn,
        keep_versions,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient};
    use crate::test_support::{IMAGE_ARN, ROLE, project};
    use serde_json::json;
    use tempfile::TempDir;

    fn config(max: Option<usize>) -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let versions = max.map_or_else(String::new, |max| {
            format!("[microvm.image.versions]\nmax = {max}\n")
        });
        let config = project(
            directory.path(),
            &format!("{versions}[microvm.run]\n{ROLE}"),
        );
        (directory, config)
    }

    fn keeping(keep_versions: usize) -> PruneOptions {
        PruneOptions {
            settings: PruneSettings {
                keep_versions: Some(keep_versions),
            },
        }
    }

    fn pruned(keep: usize) -> Call {
        Call::Prune(Arn::parse(IMAGE_ARN).unwrap(), keep)
    }

    #[tokio::test]
    async fn prune_keeps_as_many_versions_as_the_project_allows() {
        let client = FakeMicroVmClient::default();
        let (_directory, config) = config(Some(3));

        let result = prune(&PruneOptions::default(), &config, &client)
            .await
            .unwrap();

        assert_eq!(result.keep_versions, 3);
        assert_eq!(
            client.calls(),
            [pruned(3)],
            "no version is published, so none is set aside for one"
        );
    }

    #[tokio::test]
    async fn the_flag_overrides_the_project_file() {
        let client = FakeMicroVmClient::default();
        let (_directory, config) = config(Some(3));

        prune(&keeping(5), &config, &client).await.unwrap();

        assert_eq!(client.calls(), [pruned(5)]);
    }

    #[tokio::test]
    async fn the_flag_needs_no_project_setting() {
        let client = FakeMicroVmClient::default();
        let (_directory, config) = config(None);

        prune(&keeping(2), &config, &client).await.unwrap();

        assert_eq!(client.calls(), [pruned(2)]);
    }

    #[tokio::test]
    async fn pruning_needs_a_positive_number_of_versions_to_keep() {
        let (_directory, config) = config(None);
        for options in [PruneOptions::default(), keeping(0)] {
            let client = FakeMicroVmClient::default();

            let error = prune(&options, &config, &client).await.unwrap_err();

            assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
            assert!(client.calls().is_empty(), "{options:?}");
        }
    }

    #[tokio::test]
    async fn the_result_names_the_image_and_every_outcome() {
        let client = FakeMicroVmClient::default().pruned([Ok(Some(PruneReport {
            kept: vec!["5".into(), "4".into(), "3".into()],
            deleted: vec!["2".into()],
            in_use: vec!["1".into()],
            deactivated: vec!["1".into()],
        }))]);
        let (_directory, config) = config(Some(3));

        let result = prune(&PruneOptions::default(), &config, &client)
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "imageName": "demo",
                "imageArn": IMAGE_ARN,
                "keepVersions": 3,
                "kept": ["5", "4", "3"],
                "deleted": ["2"],
                "inUse": ["1"],
                "deactivated": ["1"],
            })
        );
        assert_eq!(
            result.human(),
            "✓ Pruned demo\n\
             Deleted:       2\n\
             Kept:          5, 4, 3\n\
             In use:        1\n\
             Left inactive: 1"
        );
    }

    #[tokio::test]
    async fn human_output_marks_empty_outcomes() {
        let client = FakeMicroVmClient::default().pruned([Ok(Some(PruneReport {
            kept: vec!["2".into()],
            ..PruneReport::default()
        }))]);
        let (_directory, config) = config(Some(1));

        let result = prune(&PruneOptions::default(), &config, &client)
            .await
            .unwrap();

        assert_eq!(
            result.human(),
            "✓ Pruned demo\n\
             Deleted:       none\n\
             Kept:          2\n\
             In use:        none"
        );
    }
}
