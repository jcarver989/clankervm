use super::error::MicroVmClientError;
use crate::{Arn, Tags};
use serde::{Deserialize, Deserializer};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageCapability {
    All,
}

impl ImageCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "ALL",
        }
    }
}

impl FromStr for ImageCapability {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ALL" => Ok(Self::All),
            _ => Err(format!("unknown image capability `{value}`")),
        }
    }
}

impl<'de> Deserialize<'de> for ImageCapability {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageHooks {
    pub port: i32,
    pub ready_timeout_seconds: i32,
    pub run_timeout_seconds: i32,
    pub terminate_timeout_seconds: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageConfiguration {
    pub base_image_arn: Arn,
    pub build_role_arn: Arn,
    pub description: String,
    pub minimum_memory_mib: Option<i32>,
    pub capabilities: Vec<ImageCapability>,
    pub egress_network_connector: Arn,
    pub hooks: ImageHooks,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishImageRequest {
    pub image_identifier: Arn,
    pub name: String,
    pub bundle: Vec<u8>,
    pub bundle_digest: String,
    pub artifact_bucket: String,
    pub configuration: ImageConfiguration,
    pub tags: Tags,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedImage {
    pub image_version: String,
    pub artifact_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectImageRequest {
    pub image_identifier: Arn,
    pub image_version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleasePhase {
    Pending,
    Ready,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedImageRelease {
    pub image_version: String,
    pub image_state: String,
    pub version_state: String,
    pub version_status: String,
    pub state_reason: Option<String>,
}

impl ObservedImageRelease {
    /// Unknown or unexpected AWS states fail fast instead of polling forever.
    pub fn phase(&self) -> ReleasePhase {
        let image_pending = matches!(
            self.image_state.as_str(),
            "CREATING" | "CREATED" | "UPDATING" | "UPDATED"
        );
        let version_pending = matches!(
            self.version_state.as_str(),
            "PENDING" | "IN_PROGRESS" | "SUCCESSFUL"
        );
        let ready = matches!(self.image_state.as_str(), "CREATED" | "UPDATED")
            && self.version_state == "SUCCESSFUL"
            && self.version_status == "ACTIVE";
        if ready {
            ReleasePhase::Ready
        } else if image_pending && version_pending {
            ReleasePhase::Pending
        } else {
            ReleasePhase::Failed
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneImageVersionsRequest {
    pub image_identifier: Arn,
    pub versions_to_keep: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMicroVmRequest {
    pub image_identifier: Arn,
    pub image_version: Option<String>,
    pub execution_role_arn: Arn,
    pub ingress_network_connector: Arn,
    pub egress_network_connector: Arn,
    pub run_hook_payload: String,
    pub maximum_duration_seconds: i32,
    pub client_token: Option<String>,
    pub cloudwatch_log_group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMicroVmResponse {
    pub microvm_id: String,
    pub image_version: String,
}

#[allow(async_fn_in_trait)]
pub trait MicroVmClient: Send + Sync {
    async fn publish_image(
        &self,
        request: PublishImageRequest,
    ) -> Result<PublishedImage, MicroVmClientError>;

    async fn inspect_image(
        &self,
        request: InspectImageRequest,
    ) -> Result<Option<ObservedImageRelease>, MicroVmClientError>;

    async fn prune_image_versions(
        &self,
        request: PruneImageVersionsRequest,
    ) -> Result<(), MicroVmClientError>;

    async fn run_microvm(
        &self,
        request: RunMicroVmRequest,
    ) -> Result<RunMicroVmResponse, MicroVmClientError>;
}
