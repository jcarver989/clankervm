use regex::{Captures, Regex};
#[cfg(test)]
use std::collections::BTreeMap;
use std::env;
use std::sync::LazyLock;
use thiserror::Error;

type Lookup = Box<dyn Fn(&str) -> Option<String>>;

pub(crate) struct Vars {
    lookup: Lookup,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("environment variable `{0}` is not set")]
pub(crate) struct MissingVar(pub String);

impl Vars {
    pub(crate) fn environment() -> Self {
        Self {
            lookup: Box::new(|name| env::var(name).ok()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_values(values: &[(&str, &str)]) -> Self {
        let values: BTreeMap<String, String> = values
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        Self {
            lookup: Box::new(move |name| values.get(name).cloned()),
        }
    }

    pub(crate) fn expand(&self, template: &str) -> Result<String, MissingVar> {
        let mut missing = None;
        let expanded = REFERENCE.replace_all(template, |captures: &Captures| {
            let Some(name) = var_name(captures) else {
                return "$".to_owned();
            };
            (self.lookup)(name).unwrap_or_else(|| {
                missing.get_or_insert_with(|| name.to_owned());
                captures[0].to_owned()
            })
        });
        match missing {
            Some(name) => Err(MissingVar(name)),
            None => Ok(expanded.into_owned()),
        }
    }
}

static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$(?:\$|\{([^}]+)\}|([A-Za-z_][A-Za-z0-9_]*))").expect("valid regex")
});

fn var_name<'a>(captures: &Captures<'a>) -> Option<&'a str> {
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|name| name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_bare_and_braced_references() {
        let vars = Vars::with_values(&[("ACCOUNT", "123456789012"), ("ROLE", "run")]);

        assert_eq!(
            vars.expand("arn:aws:iam::${ACCOUNT}:role/$ROLE").unwrap(),
            "arn:aws:iam::123456789012:role/run"
        );
    }

    #[test]
    fn bare_names_stop_at_the_first_non_identifier_character() {
        let vars = Vars::with_values(&[("ACCOUNT", "123"), ("ACCOUNT_2", "456")]);

        assert_eq!(
            vars.expand("bucket-$ACCOUNT-$ACCOUNT_2").unwrap(),
            "bucket-123-456"
        );
    }

    #[test]
    fn double_dollar_escapes_to_a_literal_dollar() {
        let vars = Vars::with_values(&[("HOME", "/home/me")]);

        assert_eq!(vars.expand("$$HOME $${HOME}").unwrap(), "$HOME ${HOME}");
    }

    #[test]
    fn missing_variables_are_errors() {
        let vars = Vars::with_values(&[("SET", "value")]);

        assert_eq!(
            vars.expand("$SET ${UNSET}"),
            Err(MissingVar("UNSET".into()))
        );
    }

    #[test]
    fn empty_values_are_not_missing() {
        let vars = Vars::with_values(&[("EMPTY", "")]);

        assert_eq!(vars.expand("[${EMPTY}]").unwrap(), "[]");
    }

    #[test]
    fn text_without_references_is_unchanged() {
        let vars = Vars::with_values(&[]);

        for text in ["", "plain", "trailing$", "${unclosed", "${}", "{url}", "$1"] {
            assert_eq!(vars.expand(text).unwrap(), text);
        }
    }

    #[test]
    fn substituted_values_are_not_expanded_again() {
        let vars = Vars::with_values(&[("A", "$B"), ("B", "twice")]);

        assert_eq!(vars.expand("${A} $A").unwrap(), "$B $B");
    }
}
