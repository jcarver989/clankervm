use crate::ClankerError;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tags(BTreeMap<String, String>);

impl Tags {
    pub fn parse(values: &[String]) -> Result<Self, ClankerError> {
        let mut tags = BTreeMap::new();
        for value in values {
            let (key, tag_value) = value.split_once('=').ok_or_else(|| {
                ClankerError::InvalidConfig(format!("invalid tag `{value}`; expected key=value"))
            })?;
            if key.is_empty() || tag_value.is_empty() {
                return Err(ClankerError::InvalidConfig(format!(
                    "invalid tag `{value}`; key and value must not be empty"
                )));
            }
            if tags.insert(key.into(), tag_value.into()).is_some() {
                return Err(ClankerError::InvalidConfig(format!(
                    "duplicate tag key `{key}`"
                )));
            }
        }
        Ok(Self(tags))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}
