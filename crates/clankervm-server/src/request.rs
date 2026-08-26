use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunHookRequest {
    pub microvm_id: String,
    #[serde(deserialize_with = "deserialize_run_hook_payload")]
    pub run_hook_payload: RunHookPayload,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "RawRunHookPayload")]
pub struct RunHookPayload {
    command: String,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawRunHookPayload {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

#[derive(Debug, Error)]
enum ParseRunHookPayloadError {
    #[error("command must not be blank")]
    BlankCommand,
    #[error("command must not contain NUL bytes")]
    CommandContainsNul,
    #[error("arguments must not contain NUL bytes")]
    ArgumentContainsNul,
    #[error("environment contains an invalid key or value")]
    InvalidEnvironment,
}

impl RunHookPayload {
    pub(crate) fn into_parts(self) -> (String, Vec<String>, BTreeMap<String, String>) {
        (self.command, self.args, self.environment)
    }
}

impl TryFrom<RawRunHookPayload> for RunHookPayload {
    type Error = ParseRunHookPayloadError;

    fn try_from(raw: RawRunHookPayload) -> Result<Self, Self::Error> {
        if raw.command.trim().is_empty() {
            return Err(ParseRunHookPayloadError::BlankCommand);
        }
        if raw.command.contains('\0') {
            return Err(ParseRunHookPayloadError::CommandContainsNul);
        }
        if raw.args.iter().any(|argument| argument.contains('\0')) {
            return Err(ParseRunHookPayloadError::ArgumentContainsNul);
        }
        if raw
            .environment
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
        {
            return Err(ParseRunHookPayloadError::InvalidEnvironment);
        }

        Ok(Self {
            command: raw.command,
            args: raw.args,
            environment: raw.environment,
        })
    }
}

fn deserialize_run_hook_payload<'a, T: Deserializer<'a>>(
    deserializer: T,
) -> Result<RunHookPayload, T::Error> {
    let encoded = String::deserialize(deserializer)?;
    serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
}
