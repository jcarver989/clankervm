use serde::Serialize;
use std::collections::BTreeMap;
use thiserror::Error;

const MAX_PAYLOAD_BYTES: usize = 4096;

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("{field} must not contain NUL bytes")]
    InvalidNul { field: &'static str },
    #[error("environment contains an invalid key or value")]
    InvalidEnvironment,
    #[error("run hook payload is {size} bytes; AWS allows at most {limit}")]
    TooLarge { size: usize, limit: usize },
}

pub fn build_run_payload(
    command: &str,
    args: &[String],
    region: &str,
) -> Result<String, PayloadError> {
    build_run_payload_with_environment(command, args, BTreeMap::new(), region)
}

pub fn build_run_payload_with_environment(
    command: &str,
    args: &[String],
    mut environment: BTreeMap<String, String>,
    region: &str,
) -> Result<String, PayloadError> {
    if command.contains('\0') {
        return Err(PayloadError::InvalidNul { field: "command" });
    }
    if args.iter().any(|argument| argument.contains('\0')) {
        return Err(PayloadError::InvalidNul { field: "arguments" });
    }

    if environment
        .iter()
        .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
    {
        return Err(PayloadError::InvalidEnvironment);
    }
    environment.insert("AWS_DEFAULT_REGION".into(), region.into());
    environment.insert("AWS_REGION".into(), region.into());
    let payload = serde_json::to_string(&Payload {
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

#[derive(Serialize)]
struct Payload<'a> {
    command: &'a str,
    args: &'a [String],
    environment: BTreeMap<String, String>,
}
