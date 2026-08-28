use crate::client::{MicroVmClient, RunMicroVmRequest};
use crate::config::ProjectConfig;
use crate::output::render;
use crate::payload::build_run_payload;
use crate::util::{non_empty_string, required_string};
use crate::{Arn, ClankerError, OutputFormat, Project};
use clap::Args;
use serde::{Deserialize, Serialize};

const DEFAULT_INGRESS: &str = "NO_INGRESS";
const DEFAULT_EGRESS: &str = "INTERNET_EGRESS";
const DEFAULT_MAX_DURATION: i32 = 3600;

#[derive(Debug, Default, Args)]
pub struct RunArgs {
    #[arg(long)]
    pub release: Option<String>,
    #[arg(long)]
    pub client_token: Option<String>,
    #[command(flatten)]
    pub config: RunConfig,
    #[arg(last = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Default, Args, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct RunConfig {
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
    #[arg(long)]
    pub log_group: Option<String>,
}

impl RunConfig {
    pub fn overlay(self, lower: &Self) -> Self {
        Self {
            command: self.command.or_else(|| lower.command.clone()),
            execution_role_arn: self
                .execution_role_arn
                .or_else(|| lower.execution_role_arn.clone()),
            ingress: self.ingress.or_else(|| lower.ingress.clone()),
            egress: self.egress.or_else(|| lower.egress.clone()),
            max_duration: self.max_duration.or(lower.max_duration),
            log_group: self.log_group.or_else(|| lower.log_group.clone()),
        }
    }

    fn resolve(self) -> Result<ResolvedRunConfig, ClankerError> {
        Ok(ResolvedRunConfig {
            command: self.command,
            execution_role_arn: required_string(self.execution_role_arn, "run.execution-role-arn")?,
            ingress: non_empty_string(self.ingress, "run.ingress")?
                .unwrap_or_else(|| DEFAULT_INGRESS.into()),
            egress: non_empty_string(self.egress, "run.egress")?
                .unwrap_or_else(|| DEFAULT_EGRESS.into()),
            max_duration: self.max_duration.unwrap_or(DEFAULT_MAX_DURATION),
            log_group: non_empty_string(self.log_group, "run.log-group")?,
        })
    }
}

struct ResolvedRunConfig {
    command: Option<Vec<String>>,
    execution_role_arn: String,
    ingress: String,
    egress: String,
    max_duration: i32,
    log_group: Option<String>,
}

pub(super) async fn execute<T: MicroVmClient>(
    args: RunArgs,
    project: &Project,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let result = run(args, &project.config, client).await?;
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
    args: RunArgs,
    config: &ProjectConfig,
    client: &T,
) -> Result<RunResult, ClankerError> {
    let image = config.resolve_image(args.release.as_deref())?;
    let region = &image.region;
    let run = args.config.overlay(&image.run).resolve()?;
    let command = if args.command.is_empty() {
        run.command.as_deref().unwrap_or_default()
    } else {
        &args.command
    };
    let (command, command_args) = command.split_first().ok_or_else(|| {
        ClankerError::InvalidConfig(
            "run command is required; pass it after `--` or set run.command".into(),
        )
    })?;
    if command.trim().is_empty() {
        return Err(ClankerError::InvalidConfig(
            "run.command executable cannot be empty".into(),
        ));
    }
    let payload = build_run_payload(command, command_args, region)?;
    let account_role = image
        .push
        .build_role_arn
        .as_deref()
        .unwrap_or(&run.execution_role_arn);
    let target = image.target(account_role)?;
    let execution_role = Arn::parse(&run.execution_role_arn)?;
    let log_group = run.log_group.clone();
    let request = RunMicroVmRequest {
        image_identifier: target.image_arn,
        image_version: target.version,
        execution_role_arn: execution_role,
        ingress_network_connector: Arn::network_connector(region, &run.ingress)?,
        egress_network_connector: Arn::network_connector(region, &run.egress)?,
        run_hook_payload: payload,
        maximum_duration_seconds: run.max_duration,
        client_token: args.client_token,
        cloudwatch_log_group: log_group.clone(),
    };
    let output = client.run_microvm(request).await?;
    Ok(RunResult {
        microvm_id: output.microvm_id,
        image_version: output.image_version,
        log_group,
        image_name: target.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FakeMicroVmClient, MicroVmCall, RunMicroVmResponse};
    use crate::test_support::ProjectConfigBuilder;

    fn config(run: RunConfig) -> ProjectConfig {
        ProjectConfigBuilder::new().run(run).build()
    }

    #[tokio::test]
    async fn run_applies_project_defaults() {
        let config = config(RunConfig {
            execution_role_arn: Some("arn:aws:iam::123456789012:role/run".into()),
            log_group: Some("/demo/runs".into()),
            ..RunConfig::default()
        });
        let client = FakeMicroVmClient::builder()
            .run_responses([Ok(RunMicroVmResponse {
                microvm_id: "microvm-7".into(),
                image_version: "3".into(),
            })])
            .build();
        let args = RunArgs {
            command: vec!["echo".into(), "hello world".into()],
            ..RunArgs::default()
        };

        let result = run(args, &config, &client).await.unwrap();

        assert_eq!(result.microvm_id, "microvm-7");
        assert_eq!(result.image_version, "3");
        assert_eq!(result.log_group.as_deref(), Some("/demo/runs"));
        let calls = client.calls();
        let MicroVmCall::RunMicroVm(request) = &calls[0] else {
            panic!("expected run call, got {calls:?}");
        };
        assert_eq!(
            request.image_identifier.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(request.image_version, None);
        assert!(
            request
                .ingress_network_connector
                .as_str()
                .ends_with("NO_INGRESS")
        );
        assert!(
            request
                .egress_network_connector
                .as_str()
                .ends_with("INTERNET_EGRESS")
        );
        assert_eq!(request.maximum_duration_seconds, 3600);
        assert_eq!(request.cloudwatch_log_group.as_deref(), Some("/demo/runs"));
        let payload: serde_json::Value = serde_json::from_str(&request.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "echo");
        assert_eq!(payload["args"], serde_json::json!(["hello world"]));
        assert_eq!(payload["environment"]["AWS_REGION"], "us-east-1");
    }

    #[tokio::test]
    async fn configured_command_is_used_when_the_cli_command_is_omitted() {
        let config = config(RunConfig {
            command: Some(vec!["echo".into(), "from config".into()]),
            execution_role_arn: Some("arn:aws:iam::123456789012:role/run".into()),
            ..RunConfig::default()
        });
        let client = FakeMicroVmClient::default();

        run(RunArgs::default(), &config, &client).await.unwrap();

        let calls = client.calls();
        let MicroVmCall::RunMicroVm(request) = &calls[0] else {
            panic!("expected run call, got {calls:?}");
        };
        let payload: serde_json::Value = serde_json::from_str(&request.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "echo");
        assert_eq!(payload["args"], serde_json::json!(["from config"]));
    }

    #[tokio::test]
    async fn flags_override_defaults_and_pin_the_release() {
        let config = config(RunConfig {
            command: Some(vec!["configured-command".into()]),
            execution_role_arn: Some("arn:aws:iam::123456789012:role/run".into()),
            ..RunConfig::default()
        });
        let client = FakeMicroVmClient::default();
        let args = RunArgs {
            release: Some("demo@7".into()),
            client_token: Some("run-42".into()),
            config: RunConfig {
                ingress: Some("PUBLIC_INGRESS".into()),
                max_duration: Some(60),
                ..RunConfig::default()
            },
            command: vec!["echo".into()],
        };

        run(args, &config, &client).await.unwrap();

        let calls = client.calls();
        let MicroVmCall::RunMicroVm(request) = &calls[0] else {
            panic!("expected run call, got {calls:?}");
        };
        assert_eq!(
            request.image_identifier.as_str(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
        );
        assert_eq!(request.image_version.as_deref(), Some("7"));
        assert!(
            request
                .ingress_network_connector
                .as_str()
                .ends_with("PUBLIC_INGRESS")
        );
        assert_eq!(request.maximum_duration_seconds, 60);
        assert_eq!(request.client_token.as_deref(), Some("run-42"));
        let payload: serde_json::Value = serde_json::from_str(&request.run_hook_payload).unwrap();
        assert_eq!(payload["command"], "echo");
    }

    #[test]
    fn empty_run_values_are_rejected() {
        for config in [
            RunConfig {
                execution_role_arn: Some(String::new()),
                ..RunConfig::default()
            },
            RunConfig {
                execution_role_arn: Some("arn:aws:iam::123456789012:role/run".into()),
                ingress: Some(" ".into()),
                ..RunConfig::default()
            },
        ] {
            assert!(matches!(
                config.resolve(),
                Err(ClankerError::InvalidConfig(_))
            ));
        }
    }

    #[tokio::test]
    async fn missing_command_is_rejected() {
        let config = config(RunConfig {
            execution_role_arn: Some("arn:aws:iam::123456789012:role/run".into()),
            ..RunConfig::default()
        });
        let client = FakeMicroVmClient::default();

        let error = run(RunArgs::default(), &config, &client).await.unwrap_err();

        assert!(
            error.to_string().contains("run command is required"),
            "{error}"
        );
        assert!(client.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_execution_role_is_rejected() {
        let config = config(RunConfig::default());
        let client = FakeMicroVmClient::default();
        let args = RunArgs {
            command: vec!["echo".into()],
            ..RunArgs::default()
        };

        let error = run(args, &config, &client).await.unwrap_err();

        assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
        assert!(client.calls().is_empty());
    }
}
