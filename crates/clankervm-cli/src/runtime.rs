use crate::client::{MicroVmClient, RunMicroVmRequest};
use crate::commands::image_account;
use crate::config::ProjectConfig;
use crate::{ClankerError, RunArgs, image_arn};
use serde::Serialize;
use std::collections::BTreeMap;
use thiserror::Error;

const MAX_PAYLOAD_BYTES: usize = 4096;

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("{field} must not contain NUL bytes")]
    InvalidNul { field: &'static str },
    #[error("run hook payload is {size} bytes; AWS allows at most {limit}")]
    TooLarge { size: usize, limit: usize },
}

pub fn build_run_payload(
    command: &str,
    args: &[String],
    region: &str,
) -> Result<Vec<u8>, PayloadError> {
    if command.contains('\0') {
        return Err(PayloadError::InvalidNul { field: "command" });
    }
    if args.iter().any(|argument| argument.contains('\0')) {
        return Err(PayloadError::InvalidNul { field: "arguments" });
    }

    let environment = BTreeMap::from([("AWS_DEFAULT_REGION", region), ("AWS_REGION", region)]);
    let payload = serde_json::to_vec(&Payload {
        command,
        args,
        environment,
    })
    .expect("payload serialization cannot fail");
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(PayloadError::TooLarge {
            size: payload.len(),
            limit: MAX_PAYLOAD_BYTES,
        });
    }
    Ok(payload)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub microvm_id: String,
    pub image_version: String,
    pub log_group: Option<String>,
}

pub async fn run<T: MicroVmClient>(
    args: RunArgs,
    config: &ProjectConfig,
    client: &T,
) -> Result<RunResult, ClankerError> {
    let region = &config.app.region;
    let (command, command_args) = args
        .command
        .split_first()
        .expect("clap requires a command after --");
    let payload = String::from_utf8(build_run_payload(command, command_args, region)?)
        .expect("JSON is UTF-8");
    let execution_role = args
        .execution_role_arn
        .filter(|role| !role.is_empty())
        .or_else(|| {
            config
                .run
                .execution_role_arn
                .clone()
                .filter(|role| !role.is_empty())
        })
        .ok_or_else(|| {
            ClankerError::InvalidConfig("run.execution-role-arn must be configured".into())
        })?;
    let account = image_account(config, Some(&execution_role))?;
    let (image_name, image_version) = match args.release.as_deref() {
        Some(release) => {
            let (name, version) = release
                .rsplit_once('@')
                .ok_or_else(|| ClankerError::InvalidRelease(release.into()))?;
            (name, Some(version.to_owned()))
        }
        None => (config.app.name.as_str(), None),
    };
    let ingress = args
        .ingress
        .as_deref()
        .or(config.run.ingress.as_deref())
        .unwrap_or("NO_INGRESS");
    let egress = args
        .egress
        .as_deref()
        .or(config.run.egress.as_deref())
        .unwrap_or("INTERNET_EGRESS");
    let max_duration = args
        .max_duration
        .or(config.run.max_duration)
        .unwrap_or(3600);
    let log_group = args.log_group.or_else(|| config.run.log_group.clone());
    let request = RunMicroVmRequest {
        image_identifier: image_arn(image_name, region, Some(account))?,
        image_version,
        execution_role_arn: execution_role,
        ingress_network_connector: network_connector_arn(region, ingress),
        egress_network_connector: network_connector_arn(region, egress),
        run_hook_payload: payload,
        maximum_duration_seconds: max_duration,
        client_token: args.client_token,
        cloudwatch_log_group: log_group.clone(),
    };
    let output = client.run_microvm(request).await?;
    Ok(RunResult {
        microvm_id: output.microvm_id,
        image_version: output.image_version,
        log_group,
    })
}

#[derive(Serialize)]
struct Payload<'a> {
    command: &'a str,
    args: &'a [String],
    environment: BTreeMap<&'static str, &'a str>,
}

pub(crate) fn network_connector_arn(region: &str, connector: &str) -> String {
    if connector.starts_with("arn:") {
        connector.to_owned()
    } else {
        format!("arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:{connector}")
    }
}
