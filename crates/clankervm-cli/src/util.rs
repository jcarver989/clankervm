use crate::ClankerError;
use base16ct::lower::encode_string;
use regex::{Captures, Regex};
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::sync::LazyLock;
use std::time::Duration;

static REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex"));

/// How often AWS is polled for asynchronous state changes.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn parse_release(value: &str) -> Result<(&str, &str), ClankerError> {
    value.rsplit_once('@').ok_or_else(|| {
        ClankerError::InvalidConfig(format!("invalid release `{value}`; expected NAME@VERSION"))
    })
}

pub(crate) fn positive_duration(value: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("must be greater than zero".into());
    }
    Ok(duration)
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

/// Pruning always leaves at least one launchable version to fall back to.
pub(crate) fn validate_keep_versions(keep: Option<usize>) -> Result<(), ClankerError> {
    if keep == Some(0) {
        return Err(ClankerError::InvalidConfig(
            "microvm.image.versions.max must be at least 1".into(),
        ));
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

/// Replaces `${NAME}` references; substituted values are not expanded again.
fn expand(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut missing = None;
    let expanded = REFERENCE.replace_all(text, |captures: &Captures| {
        lookup(&captures[1]).unwrap_or_else(|| {
            missing.get_or_insert_with(|| captures[1].to_owned());
            String::new()
        })
    });
    match missing {
        Some(name) => Err(format!("environment variable `{name}` is not set")),
        None => Ok(expanded.into_owned()),
    }
}

fn expand_env(text: &str) -> Result<String, String> {
    expand(text, |name| env::var(name).ok())
}

/// Deserializes an optional setting, expanding environment variable references.
pub(crate) fn env_string<'de, T: Deserializer<'de>>(
    deserializer: T,
) -> Result<Option<String>, T::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|text| expand_env(&text))
        .transpose()
        .map_err(T::Error::custom)
}

/// Deserializes an optional list, expanding environment variable references.
pub(crate) fn env_strings<'de, T: Deserializer<'de>>(
    deserializer: T,
) -> Result<Option<Vec<String>>, T::Error> {
    Option::<Vec<String>>::deserialize(deserializer)?
        .map(|items| items.iter().map(|text| expand_env(text)).collect())
        .transpose()
        .map_err(T::Error::custom)
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

    #[test]
    fn expand_substitutes_braced_references_once() {
        let lookup = |name: &str| {
            [
                ("ACCOUNT", "123"),
                ("ROLE", "run"),
                ("EMPTY", ""),
                ("NESTED", "${ROLE}"),
            ]
            .into_iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_owned())
        };

        for (text, expanded) in [
            (
                "arn:aws:iam::${ACCOUNT}:role/${ROLE}",
                "arn:aws:iam::123:role/run",
            ),
            ("${ACCOUNT}${ROLE}", "123run"),
            ("[${EMPTY}]", "[]"),
            ("${NESTED}", "${ROLE}"),
            (
                "$ACCOUNT ${} ${unclosed {url}",
                "$ACCOUNT ${} ${unclosed {url}",
            ),
        ] {
            assert_eq!(expand(text, lookup).as_deref(), Ok(expanded), "{text}");
        }
        assert_eq!(
            expand("${ACCOUNT}-${UNSET}", lookup),
            Err("environment variable `UNSET` is not set".into())
        );
    }
}
