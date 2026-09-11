use crate::ClankerError;
use serde::Serialize;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct Arn(String);

impl Arn {
    pub(crate) fn parse(value: &str) -> Result<Self, ClankerError> {
        if !is_valid(value) {
            return Err(ClankerError::InvalidConfig(format!(
                "invalid ARN `{value}`"
            )));
        }
        Ok(Self(value.into()))
    }

    /// A Lambda MicroVMs resource: a full ARN is used as-is, a bare name is expanded.
    pub(crate) fn lambda(
        region: &str,
        account: &str,
        resource_type: &str,
        resource: &str,
    ) -> Result<Self, ClankerError> {
        if resource.starts_with("arn:") {
            return Self::parse(resource);
        }
        Ok(Self(format!(
            "arn:aws:lambda:{region}:{account}:{resource_type}:{resource}"
        )))
    }

    pub(crate) fn network_connector(region: &str, connector: &str) -> Result<Self, ClankerError> {
        let resource = if connector.starts_with("arn:") {
            connector.to_owned()
        } else {
            format!("aws-network-connector:{connector}")
        };
        Self::lambda(region, "aws", "network-connector", &resource)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The region of `arn:partition:service:region:account:resource`.
    pub(crate) fn region(&self) -> Option<&str> {
        self.0.split(':').nth(3).filter(|region| !region.is_empty())
    }

    /// The account id of `arn:partition:service:region:account:resource`.
    pub(crate) fn account(&self) -> Option<&str> {
        self.0
            .split(':')
            .nth(4)
            .filter(|account| !account.is_empty())
    }
}

/// `arn:partition:service:region:account:resource` — partition, service, and
/// resource must be non-empty; region and account may be empty; the resource
/// may contain further colons.
fn is_valid(value: &str) -> bool {
    let mut parts = value.splitn(6, ':');
    parts.next() == Some("arn")
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some()
        && parts.next().is_some()
        && parts.next().is_some_and(|part| !part.is_empty())
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
    fn account_is_the_fifth_component() {
        let arn = Arn::parse("arn:aws:lambda:us-east-1:123456789012:microvm-image:demo").unwrap();
        assert_eq!(arn.account(), Some("123456789012"));

        assert_eq!(
            Arn::parse("arn:aws:lambda:us-east-1::image")
                .unwrap()
                .account(),
            None
        );
    }

    #[test]
    fn region_is_the_fourth_component() {
        let arn = Arn::parse("arn:aws:lambda:us-east-1:123456789012:microvm-image:demo").unwrap();
        assert_eq!(arn.region(), Some("us-east-1"));

        assert_eq!(
            Arn::parse("arn:aws:lambda::123:image").unwrap().region(),
            None
        );
    }

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
            Arn::lambda("us-east-1", "123", "microvm-image", "demo")
                .unwrap()
                .as_str(),
            "arn:aws:lambda:us-east-1:123:microvm-image:demo"
        );
        assert_eq!(
            Arn::lambda(
                "us-east-1",
                "aws",
                "microvm-image",
                "arn:aws:lambda:eu-west-1:aws:microvm-image:al2023-1"
            )
            .unwrap()
            .as_str(),
            "arn:aws:lambda:eu-west-1:aws:microvm-image:al2023-1"
        );
        assert_eq!(
            Arn::network_connector("us-east-1", "INTERNET_EGRESS")
                .unwrap()
                .as_str(),
            "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
        );
    }
}
