use crate::ClankerError;
use regex::Regex;
use serde::Serialize;
use std::fmt;
use std::sync::LazyLock;

static ARN_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\Aarn:[^:]+:[^:]+:[^:]*:[^:]*:[\s\S]+\z").expect("valid ARN pattern")
});

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Arn(String);

impl Arn {
    pub fn parse(value: &str) -> Result<Self, ClankerError> {
        if !ARN_PATTERN.is_match(value) {
            return Err(ClankerError::InvalidArn(value.into()));
        }
        Ok(Self(value.into()))
    }

    pub fn image(
        image: &str,
        region: &str,
        account_id: Option<&str>,
    ) -> Result<Self, ClankerError> {
        if image.starts_with("arn:") {
            return Self::parse(image).map_err(|_| ClankerError::InvalidImage(image.into()));
        }

        if image.is_empty() {
            return Err(ClankerError::InvalidImage(image.into()));
        }

        let account_id = account_id.ok_or_else(|| ClankerError::InvalidImage(image.into()))?;
        Ok(Self::lambda_resource(
            region,
            account_id,
            "microvm-image",
            image,
        ))
    }

    pub fn base_image(region: &str, image: &str) -> Result<Self, ClankerError> {
        Self::resource_or_aws(region, "microvm-image", image)
    }

    pub fn network_connector(region: &str, connector: &str) -> Result<Self, ClankerError> {
        Self::resource_or_aws(
            region,
            "network-connector",
            &format!("aws-network-connector:{connector}"),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    fn resource_or_aws(
        region: &str,
        resource_type: &str,
        resource: &str,
    ) -> Result<Self, ClankerError> {
        if resource.starts_with("arn:") {
            Self::parse(resource)
        } else {
            Ok(Self::lambda_resource(
                region,
                "aws",
                resource_type,
                resource,
            ))
        }
    }

    fn lambda_resource(region: &str, account: &str, resource_type: &str, resource: &str) -> Self {
        Self(format!(
            "arn:aws:lambda:{region}:{account}:{resource_type}:{resource}"
        ))
    }
}

impl AsRef<str> for Arn {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Arn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_resources_containing_colons() {
        let arn = Arn::parse("arn:aws:lambda:us-east-1:123:function:name:alias").unwrap();
        assert_eq!(
            arn.as_str(),
            "arn:aws:lambda:us-east-1:123:function:name:alias"
        );
    }

    #[test]
    fn parse_rejects_malformed_values() {
        for value in [
            "",
            "arn:",
            "arn:aws",
            "arn::lambda:region:account:resource",
            "arn:aws::region:account:resource",
            "arn:aws:lambda:region:account:",
        ] {
            assert!(Arn::parse(value).is_err(), "accepted `{value}`");
        }
    }

    #[test]
    fn constructors_resolve_shorthand_resources() {
        assert_eq!(
            Arn::image("demo", "us-east-1", Some("123"))
                .unwrap()
                .as_str(),
            "arn:aws:lambda:us-east-1:123:microvm-image:demo"
        );
        assert_eq!(
            Arn::base_image("us-east-1", "al2023-1").unwrap().as_str(),
            "arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1"
        );
        assert_eq!(
            Arn::network_connector("us-east-1", "INTERNET_EGRESS")
                .unwrap()
                .as_str(),
            "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
        );
    }
}
