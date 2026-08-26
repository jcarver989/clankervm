use crate::{ClankerError, RunArgs, image_arn};
use aws_config::SdkConfig;
use aws_sdk_lambdamicrovms::types::{CloudWatchLogging, Logging};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use thiserror::Error;

const MAX_PAYLOAD_BYTES: usize = 4096;
pub type RuntimeEnvironment = BTreeMap<String, String>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnvironmentError {
    #[error("invalid entry at line {line}")]
    InvalidEntry { line: usize },
    #[error("duplicate key {key} at line {line}")]
    DuplicateKey { line: usize, key: String },
    #[error("invalid assignment `{0}`")]
    InvalidAssignment(String),
    #[error("environment value for {key} contains a NUL byte")]
    InvalidValue { key: String },
    #[error("host environment variable {0} is not set")]
    MissingHostVariable(String),
}

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("{field} must not contain NUL bytes")]
    InvalidNul { field: &'static str },
    #[error("environment contains an invalid key")]
    InvalidEnvironmentKey,
    #[error(
        "run hook payload is {size} bytes; AWS allows at most {limit}; environment keys: {keys}"
    )]
    TooLarge {
        size: usize,
        limit: usize,
        keys: String,
    },
}

pub fn parse_env_file(content: &str) -> Result<RuntimeEnvironment, EnvironmentError> {
    let mut result = RuntimeEnvironment::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(EnvironmentError::InvalidEntry { line: index + 1 });
        };
        insert(&mut result, key, value, index + 1)?;
    }
    Ok(result)
}

pub fn parse_env_assignments(
    assignments: &[String],
) -> Result<RuntimeEnvironment, EnvironmentError> {
    let mut result = RuntimeEnvironment::new();
    for assignment in assignments {
        if let Some((key, value)) = assignment.split_once('=') {
            insert(&mut result, key, value, 0)?;
        } else if valid_name(assignment) {
            let value = std::env::var(assignment)
                .map_err(|_| EnvironmentError::MissingHostVariable(assignment.clone()))?;
            insert(&mut result, assignment, &value, 0)?;
        } else {
            return Err(EnvironmentError::InvalidAssignment(assignment.clone()));
        }
    }
    Ok(result)
}

pub fn build_run_payload(
    command: &str,
    args: &[String],
    environment: &RuntimeEnvironment,
    region: &str,
) -> Result<Vec<u8>, PayloadError> {
    if command.contains('\0') {
        return Err(PayloadError::InvalidNul { field: "command" });
    }
    if args.iter().any(|argument| argument.contains('\0')) {
        return Err(PayloadError::InvalidNul { field: "arguments" });
    }
    if environment
        .keys()
        .any(|key| key.is_empty() || key.contains('='))
    {
        return Err(PayloadError::InvalidEnvironmentKey);
    }
    if environment
        .iter()
        .any(|(key, value)| key.contains('\0') || value.contains('\0'))
    {
        return Err(PayloadError::InvalidNul {
            field: "environment",
        });
    }

    let mut environment = environment.clone();
    environment.insert("AWS_REGION".into(), region.into());
    environment.insert("AWS_DEFAULT_REGION".into(), region.into());
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
            keys: payload_environment_keys(&payload),
        });
    }
    Ok(payload)
}

pub async fn run(
    args: RunArgs,
    region: &str,
    account_id: Option<&str>,
    sdk: &SdkConfig,
) -> Result<(), ClankerError> {
    let mut environment = if let Some(path) = &args.env_file {
        let content = fs::read_to_string(path).map_err(|source| ClankerError::Io {
            action: format!("read {}", path.display()),
            source,
        })?;
        parse_env_file(&content)?
    } else {
        RuntimeEnvironment::new()
    };
    for (key, value) in parse_env_assignments(&args.env)? {
        environment.insert(key, value);
    }
    let (command, command_args) = if let Some(script) = &args.script {
        let contents = fs::read_to_string(script).map_err(|source| ClankerError::Io {
            action: format!("read {}", script.display()),
            source,
        })?;
        let mut script_args = vec![
            "-c".into(),
            contents,
            script
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ];
        script_args.extend(args.command);
        (args.shell, script_args)
    } else if let Some((command, command_args)) = args.command.split_first() {
        (command.clone(), command_args.to_vec())
    } else {
        (args.shell, Vec::new())
    };
    let payload = String::from_utf8(
        build_run_payload(&command, &command_args, &environment, region)
            .map_err(ClankerError::Payload)?,
    )
    .expect("JSON is UTF-8");
    let request = aws_sdk_lambdamicrovms::Client::new(sdk)
        .run_microvm()
        .image_identifier(image_arn(&args.image, region, account_id)?)
        .execution_role_arn(args.execution_role_arn)
        .ingress_network_connectors(network_connector_arn(region, &args.ingress))
        .egress_network_connectors(network_connector_arn(region, &args.egress))
        .run_hook_payload(payload)
        .maximum_duration_in_seconds(args.max_duration);
    let request = if let Some(group) = args.log_group {
        request.logging(Logging::CloudWatch(
            CloudWatchLogging::builder().log_group(group).build(),
        ))
    } else {
        request
    };
    let output = request
        .send()
        .await
        .map_err(|e| ClankerError::Run(Box::new(e)))?;
    println!("{}", output.microvm_id());
    Ok(())
}

#[derive(Serialize)]
struct Payload<'a> {
    command: &'a str,
    args: &'a [String],
    environment: RuntimeEnvironment,
}
fn insert(
    map: &mut RuntimeEnvironment,
    key: &str,
    value: &str,
    line: usize,
) -> Result<(), EnvironmentError> {
    if !valid_name(key) {
        return Err(if line == 0 {
            EnvironmentError::InvalidAssignment(key.into())
        } else {
            EnvironmentError::InvalidEntry { line }
        });
    }
    if value.contains('\0') {
        return Err(EnvironmentError::InvalidValue { key: key.into() });
    }
    if map.contains_key(key) {
        return Err(EnvironmentError::DuplicateKey {
            line,
            key: key.into(),
        });
    }
    map.insert(key.into(), value.into());
    Ok(())
}
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
fn network_connector_arn(region: &str, connector: &str) -> String {
    if connector.starts_with("arn:") {
        connector.to_owned()
    } else {
        format!("arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:{connector}")
    }
}

fn payload_environment_keys(payload: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.get("environment")
                .and_then(|e| e.as_object())
                .map(|e| e.keys().cloned().collect::<Vec<_>>().join(", "))
        })
        .unwrap_or_default()
}
