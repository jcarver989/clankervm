use crate::ClankerError;
use base16ct::lower::encode_string;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(crate) fn parse_release(value: &str) -> Result<(&str, &str), ClankerError> {
    value.rsplit_once('@').ok_or_else(|| {
        ClankerError::InvalidConfig(format!("invalid release `{value}`; expected NAME@VERSION"))
    })
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    encode_string(Sha256::digest(bytes).as_ref())
}

pub(crate) fn validate_non_empty(value: Option<&str>, name: &str) -> Result<(), ClankerError> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(ClankerError::InvalidConfig(format!(
            "{name} must not be empty"
        )));
    }
    Ok(())
}

pub(crate) fn required<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, ClankerError> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ClankerError::InvalidConfig(format!(
            "{name} must be configured"
        ))),
    }
}

pub(crate) fn parse_key_values(
    values: &[String],
    label: &str,
    require_value: bool,
) -> Result<BTreeMap<String, String>, ClankerError> {
    let mut pairs = BTreeMap::new();
    for value in values {
        let (key, pair) = value.split_once('=').ok_or_else(|| {
            ClankerError::InvalidConfig(format!("invalid {label} `{value}`; expected key=value"))
        })?;
        if key.is_empty()
            || key.contains(['=', '\0'])
            || pair.contains('\0')
            || (require_value && pair.is_empty())
        {
            return Err(ClankerError::InvalidConfig(format!(
                "invalid {label} `{value}`"
            )));
        }
        if pairs.insert(key.into(), pair.into()).is_some() {
            return Err(ClankerError::InvalidConfig(format!(
                "duplicate {label} key `{key}`"
            )));
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_values_reject_malformed_pairs_and_only_tags_require_a_value() {
        for values in [
            vec!["missing-equals".into()],
            vec!["=value".into()],
            vec!["A=1".into(), "A=2".into()],
        ] {
            assert!(
                parse_key_values(&values, "pair", false).is_err(),
                "accepted {values:?}"
            );
        }

        let empty = ["EMPTY=".to_owned()];
        assert!(parse_key_values(&empty, "tag", true).is_err());
        let environment = parse_key_values(&empty, "environment variable", false).unwrap();
        assert_eq!(environment.get("EMPTY").map(String::as_str), Some(""));
    }
}
