use crate::ClankerError;
use base16ct::lower::encode_string;
use sha2::{Digest, Sha256};

pub(crate) fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

pub(crate) fn parse_release(value: &str) -> Result<(&str, &str), ClankerError> {
    value
        .rsplit_once('@')
        .ok_or_else(|| ClankerError::InvalidRelease(value.into()))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    encode_string(Sha256::digest(bytes).as_ref())
}
