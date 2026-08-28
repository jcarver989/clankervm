use crate::ClankerError;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunEnvironment(BTreeMap<String, String>);

impl RunEnvironment {
    pub(crate) fn parse(values: &[String]) -> Result<Self, ClankerError> {
        let mut environment = BTreeMap::new();
        for value in values {
            let (key, environment_value) = value.split_once('=').ok_or_else(|| {
                ClankerError::InvalidConfig(format!(
                    "invalid environment variable `{value}`; expected key=value"
                ))
            })?;
            if key.is_empty() || key.contains(['=', '\0']) || environment_value.contains('\0') {
                return Err(ClankerError::InvalidConfig(format!(
                    "invalid environment variable `{value}`"
                )));
            }
            if environment
                .insert(key.into(), environment_value.into())
                .is_some()
            {
                return Err(ClankerError::InvalidConfig(format!(
                    "duplicate environment variable `{key}`"
                )));
            }
        }
        Ok(Self(environment))
    }

    pub(crate) fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_and_preserves_empty_values() {
        let environment = RunEnvironment::parse(&["GREETING=hello".into(), "EMPTY=".into()])
            .unwrap()
            .into_inner();

        assert_eq!(
            environment.get("GREETING").map(String::as_str),
            Some("hello")
        );
        assert_eq!(environment.get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn rejects_invalid_and_duplicate_values() {
        for values in [
            vec!["NO_EQUALS".into()],
            vec!["=value".into()],
            vec!["A=1".into(), "A=2".into()],
        ] {
            assert!(RunEnvironment::parse(&values).is_err());
        }
    }
}
