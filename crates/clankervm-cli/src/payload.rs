use crate::ClankerError;
use serde::Serialize;
use std::collections::BTreeMap;

const MAX_PAYLOAD_BYTES: usize = 4096;

/// The run hook payload the guest expects, with the AWS region exported.
pub(crate) fn build_run_payload(
    command: &str,
    args: &[String],
    environment: BTreeMap<String, String>,
    region: &str,
) -> Result<String, ClankerError> {
    build_payload(command, args, environment, region, "run")
}

pub(crate) fn build_image_hook_payload(
    command: &str,
    args: &[String],
    environment: BTreeMap<String, String>,
    region: &str,
    hook: &str,
) -> Result<String, ClankerError> {
    if command.trim().is_empty() {
        return Err(ClankerError::InvalidConfig(format!(
            "microvm.image.hooks.{hook}.command executable cannot be empty"
        )));
    }
    if environment
        .iter()
        .any(|(key, value)| key.contains('\0') || value.contains('\0'))
    {
        return Err(ClankerError::InvalidConfig(format!(
            "{hook} environment contains NUL bytes"
        )));
    }
    build_payload(command, args, environment, region, hook)
}

fn build_payload(
    command: &str,
    args: &[String],
    mut environment: BTreeMap<String, String>,
    region: &str,
    hook: &str,
) -> Result<String, ClankerError> {
    if command.contains('\0') || args.iter().any(|argument| argument.contains('\0')) {
        return Err(ClankerError::InvalidConfig(format!(
            "{hook} command and arguments must not contain NUL bytes"
        )));
    }
    environment.insert("AWS_DEFAULT_REGION".into(), region.into());
    environment.insert("AWS_REGION".into(), region.into());
    let payload = serde_json::to_string(&Payload {
        command,
        args,
        environment,
    })?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(ClankerError::InvalidConfig(format!(
            "{hook} hook payload is {} bytes; AWS allows at most {MAX_PAYLOAD_BYTES}",
            payload.len()
        )));
    }
    Ok(payload)
}

#[derive(Serialize)]
struct Payload<'a> {
    command: &'a str,
    args: &'a [String],
    environment: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_preserves_the_command_and_exports_the_region() {
        let payload = build_run_payload(
            "echo",
            &["hello world".into()],
            BTreeMap::new(),
            "us-east-1",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["command"], "echo");
        assert_eq!(json["args"], serde_json::json!(["hello world"]));
        assert_eq!(json["environment"]["AWS_REGION"], "us-east-1");
    }

    #[test]
    fn oversized_and_nul_payloads_are_rejected() {
        for (command, argument) in [("sh", "x".repeat(5000)), ("sh\0", String::new())] {
            let error =
                build_run_payload(command, &[argument], BTreeMap::new(), "region").unwrap_err();
            assert!(matches!(error, ClankerError::InvalidConfig(_)), "{error}");
        }
    }
}
