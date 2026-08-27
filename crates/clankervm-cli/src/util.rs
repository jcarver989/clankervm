use crate::ClankerError;
use base16ct::lower::encode_string;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(crate) fn parse_release(value: &str) -> Result<(&str, &str), ClankerError> {
    value
        .rsplit_once('@')
        .ok_or_else(|| ClankerError::InvalidRelease(value.into()))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    encode_string(Sha256::digest(bytes).as_ref())
}

pub(crate) fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

pub(crate) fn validate_non_empty(value: Option<&str>, name: &str) -> Result<(), ClankerError> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(ClankerError::InvalidConfig(format!(
            "{name} must not be empty"
        )));
    }
    Ok(())
}

pub(crate) fn non_empty_string(
    value: Option<String>,
    name: &str,
) -> Result<Option<String>, ClankerError> {
    validate_non_empty(value.as_deref(), name)?;
    Ok(value)
}

pub(crate) fn required_string(value: Option<String>, name: &str) -> Result<String, ClankerError> {
    non_empty_string(value, name)?
        .ok_or_else(|| ClankerError::InvalidConfig(format!("{name} must be configured")))
}

pub(crate) fn deserialize_optional_duration<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    use serde::Deserialize;

    Option::<String>::deserialize(deserializer)?
        .map(|value| humantime::parse_duration(&value).map_err(serde::de::Error::custom))
        .transpose()
}
