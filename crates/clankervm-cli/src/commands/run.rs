use crate::arn::Arn;
use crate::client::{LaunchSpec, MicroVmClient};
use crate::config::{ProjectConfig, Settings};
use crate::output::render;
use crate::payload::build_run_payload;
use crate::util::{parse_key_values, required, validate_non_empty};
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const DEFAULT_INGRESS: &str = "NO_INGRESS";
const DEFAULT_RUN_EGRESS: &str = "INTERNET_EGRESS";
const DEFAULT_MAX_DURATION: i32 = 3600;

#[derive(Debug, Default, Args)]
pub struct RunOptions {
    #[arg(last = true, allow_hyphen_values = true)]
    pub arguments: Vec<String>,
    #[arg(long)]
    pub release: Option<String>,
    #[arg(long)]
    pub client_token: Option<String>,
    #[command(flatten)]
    pub settings: RunSettings,
}

/// Run settings, shared by `--flags` and the `[run]` table.
#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct RunSettings {
    #[arg(skip)]
    pub command: Option<Vec<String>>,
    #[arg(long)]
    pub execution_role_arn: Option<String>,
    #[arg(long)]
    pub ingress: Option<String>,
    #[arg(long)]
    pub egress: Option<String>,
    #[arg(long)]
    pub max_duration: Option<i32>,
    #[arg(long = "env")]
    pub environment: Option<Vec<String>>,
    #[arg(long)]
    pub log_group: Option<String>,
}

impl Settings for RunSettings {
    fn validate(&self) -> Result<(), ClankerError> {
        if self.command.is_some() {
            self.command(&[])?;
        }
        validate_non_empty(self.execution_role_arn.as_deref(), "run.execution-role-arn")?;
        validate_non_empty(self.ingress.as_deref(), "run.ingress")?;
        validate_non_empty(self.egress.as_deref(), "run.egress")?;
        validate_non_empty(self.log_group.as_deref(), "run.log-group")?;
        self.environment()?;
        Ok(())
    }
}

impl RunSettings {
    pub(crate) fn execution_role_arn(&self) -> Result<&str, ClankerError> {
        required(self.execution_role_arn.as_deref(), "run.execution-role-arn")
    }

    /// Run-hook environment: `key=value` pairs whose values may be empty.
    pub(crate) fn environment(&self) -> Result<BTreeMap<String, String>, ClankerError> {
        parse_key_values(
            self.environment.as_deref().unwrap_or_default(),
            "environment variable",
            false,
        )
    }

    /// The executable and arguments to run: `arguments` win over `run.command`.
    pub(crate) fn command<'a>(
        &'a self,
        arguments: &'a [String],
    ) -> Result<(&'a str, &'a [String]), ClankerError> {
        let command = if arguments.is_empty() {
            self.command.as_deref().unwrap_or_default()
        } else {
            arguments
        };
        let (executable, arguments) = command.split_first().ok_or_else(|| {
            ClankerError::InvalidConfig(
                "run command is required; pass it after `--` or set run.command".into(),
            )
        })?;
        if executable.trim().is_empty() {
            return Err(ClankerError::InvalidConfig(
                "run.command executable cannot be empty".into(),
            ));
        }
        Ok((executable, arguments))
    }

    pub(crate) fn ingress(&self) -> &str {
        self.ingress.as_deref().unwrap_or(DEFAULT_INGRESS)
    }
    pub(crate) fn egress(&self) -> &str {
        self.egress.as_deref().unwrap_or(DEFAULT_RUN_EGRESS)
    }
    pub(crate) fn max_duration(&self) -> i32 {
        self.max_duration.unwrap_or(DEFAULT_MAX_DURATION)
    }
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &RunOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let result = run(options, config, client).await?;
    render(format, &result, || {
        format!(
            "✓ Started MicroVM {}\n  Release: {}@{}{}",
            result.microvm_id,
            result.image_name,
            result.image_version,
            result
                .log_group
                .as_ref()
                .map_or_else(String::new, |group| format!("\n  Logs:    {group}"))
        )
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub microvm_id: String,
    pub image_version: String,
    pub log_group: Option<String>,
    #[serde(skip)]
    image_name: String,
}

pub async fn run<T: MicroVmClient>(
    cli: &RunOptions,
    config: &ProjectConfig,
    client: &T,
) -> Result<RunResult, ClankerError> {
    let settings = config.run.merge(&cli.settings)?;
    let execution_role_arn = Arn::parse(settings.execution_role_arn()?)?;
    let target = config.target(cli.release.as_deref(), config.account_role(&settings)?)?;
    let (command, arguments) = settings.command(&cli.arguments)?;
    let payload = build_run_payload(command, arguments, settings.environment()?, &target.region)?;
    let spec = LaunchSpec {
        image_arn: target.arn,
        image_version: target.version,
        execution_role_arn,
        ingress_network_connector: Arn::network_connector(&target.region, settings.ingress())?,
        egress_network_connector: Arn::network_connector(&target.region, settings.egress())?,
        run_hook_payload: payload,
        maximum_duration_seconds: settings.max_duration(),
        client_token: cli.client_token.clone(),
        cloudwatch_log_group: settings.log_group.clone(),
    };
    let launched = client.launch(&spec).await?;
    Ok(RunResult {
        microvm_id: launched.microvm_id,
        image_version: launched.image_version,
        log_group: settings.log_group,
        image_name: target.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient, Launch};
    use crate::test_support::{ROLE, project};
    use tempfile::TempDir;

    /// A real project file, so the `[run]` schema and precedence are covered too.
    fn config(run: &str) -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), &format!("[run]\n{run}"));
        (directory, config)
    }

    fn launched(calls: &[Call]) -> LaunchSpec {
        let [Call::Launch(spec)] = calls else {
            panic!("expected one launch, got {calls:?}");
        };
        spec.clone()
    }

    #[test]
    fn empty_run_values_are_rejected() {
        for settings in [
            RunSettings {
                execution_role_arn: Some(String::new()),
                ..RunSettings::default()
            },
            RunSettings {
                ingress: Some(" ".into()),
                ..RunSettings::default()
            },
            RunSettings {
                environment: Some(vec!["INVALID".into()]),
                ..RunSettings::default()
            },
            RunSettings {
                command: Some(Vec::new()),
                ..RunSettings::default()
            },
        ] {
            assert!(
                matches!(settings.validate(), Err(ClankerError::InvalidConfig(_))),
                "accepted {settings:?}"
            );
        }
    }

    #[test]
    fn the_command_line_wins_over_the_configured_command() {
        let settings = RunSettings {
            command: Some(vec!["configured".into()]),
            ..RunSettings::default()
        };

        let cli = ["echo".to_owned(), "cli".to_owned()];
        let (executable, arguments) = settings.command(&cli).unwrap();
        assert_eq!(executable, "echo");
        assert_eq!(arguments, ["cli"]);

        assert_eq!(settings.command(&[]).unwrap().0, "configured");
    }

    #[test]
    fn a_missing_command_is_rejected() {
        let error = RunSettings::default().command(&[]).unwrap_err();

        assert!(
            error.to_string().contains("run command is required"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn run_applies_project_defaults() {
        let (_directory, config) = config(&format!("{ROLE}log-group = \"/demo/runs\"\n"));
        let client = FakeMicroVmClient::default().launched([Ok(Launch {
            microvm_id: "microvm-7".into(),
            image_version: "3".into(),
        })]);
        let cli = RunOptions {
            arguments: vec!["echo".into(), "hello world".into()],
            ..RunOptions::default()
        };

        let result = run(&cli, &config, &client).await.unwrap();

        assert_eq!(result.microvm_id, "microvm-7");
        assert_eq!(result.image_version, "3");
        assert_eq!(result.log_group.as_deref(), Some("/demo/runs"));
        let spec = launched(&client.calls());
        assert_eq!(
            spec.image_arn.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(spec.image_version, None);
        assert!(
            spec.ingress_network_connector
                .as_str()
                .ends_with("NO_INGRESS")
        );
        assert!(
            spec.egress_network_connector
                .as_str()
                .ends_with("INTERNET_EGRESS")
        );
        assert_eq!(spec.maximum_duration_seconds, 3600);
        assert_eq!(spec.cloudwatch_log_group.as_deref(), Some("/demo/runs"));
        let payload: serde_json::Value = serde_json::from_str(&spec.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "echo");
        assert_eq!(payload["args"], serde_json::json!(["hello world"]));
        assert_eq!(payload["environment"]["AWS_REGION"], "us-east-1");
    }

    #[tokio::test]
    async fn configured_command_is_used_when_the_cli_command_is_omitted() {
        let (_directory, config) = config(&format!(
            "{ROLE}command = [\"echo\", \"from config\"]\nenvironment = [\"GREETING=hello\"]\n"
        ));
        let client = FakeMicroVmClient::default();

        run(&RunOptions::default(), &config, &client).await.unwrap();

        let spec = launched(&client.calls());
        let payload: serde_json::Value = serde_json::from_str(&spec.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "echo");
        assert_eq!(payload["args"], serde_json::json!(["from config"]));
        assert_eq!(payload["environment"]["GREETING"], "hello");
    }

    #[tokio::test]
    async fn flags_override_defaults_and_pin_the_release() {
        let (_directory, config) = config(&format!("{ROLE}command = [\"configured-command\"]\n"));
        let client = FakeMicroVmClient::default();
        let cli = RunOptions {
            arguments: vec!["echo".into()],
            release: Some("demo@7".into()),
            client_token: Some("run-42".into()),
            settings: RunSettings {
                ingress: Some("PUBLIC_INGRESS".into()),
                max_duration: Some(60),
                ..RunSettings::default()
            },
        };

        run(&cli, &config, &client).await.unwrap();

        let spec = launched(&client.calls());
        assert_eq!(spec.image_version.as_deref(), Some("7"));
        assert!(
            spec.ingress_network_connector
                .as_str()
                .ends_with("PUBLIC_INGRESS")
        );
        assert_eq!(spec.maximum_duration_seconds, 60);
        assert_eq!(spec.client_token.as_deref(), Some("run-42"));
        let payload: serde_json::Value = serde_json::from_str(&spec.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "echo");
    }

    #[tokio::test]
    async fn missing_command_is_rejected() {
        let (_directory, config) = config(ROLE);
        let client = FakeMicroVmClient::default();

        let error = run(&RunOptions::default(), &config, &client)
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("run command is required"),
            "{error}"
        );
        assert!(client.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_execution_role_is_rejected() {
        let (_directory, config) = config("");
        let client = FakeMicroVmClient::default();
        let cli = RunOptions {
            arguments: vec!["echo".into()],
            ..RunOptions::default()
        };

        let error = run(&cli, &config, &client).await.unwrap_err();

        assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
        assert!(client.calls().is_empty());
    }
}
