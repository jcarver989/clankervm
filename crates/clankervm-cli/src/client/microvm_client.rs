use super::error::MicroVmClientError;
use crate::{Arn, Tags};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageCapability {
    All,
}

impl ImageCapability {
    pub fn as_str(&self) -> &str {
        match self {
            Self::All => "ALL",
        }
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
pub enum ImageState {
    Creating,
    Created,
    Updating,
    Updated,
    Deleting,
    Deleted,
    Failed,
    Unknown(String),
}

impl ImageState {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Creating => "CREATING",
            Self::Created => "CREATED",
            Self::Updating => "UPDATING",
            Self::Updated => "UPDATED",
            Self::Deleting => "DELETING",
            Self::Deleted => "DELETED",
            Self::Failed => "FAILED",
            Self::Unknown(value) => value,
        }
    }

    pub(super) fn from_aws(value: &str) -> Self {
        match value {
            "CREATING" => Self::Creating,
            "CREATED" => Self::Created,
            "UPDATING" => Self::Updating,
            "UPDATED" => Self::Updated,
            "DELETING" => Self::Deleting,
            "DELETED" => Self::Deleted,
            "FAILED" => Self::Failed,
            _ => Self::Unknown(value.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageVersionState {
    Pending,
    InProgress,
    Successful,
    Failed,
    Unknown(String),
}

impl ImageVersionState {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "PENDING",
            Self::InProgress => "IN_PROGRESS",
            Self::Successful => "SUCCESSFUL",
            Self::Failed => "FAILED",
            Self::Unknown(value) => value,
        }
    }

    pub(super) fn from_aws(value: &str) -> Self {
        match value {
            "PENDING" => Self::Pending,
            "IN_PROGRESS" => Self::InProgress,
            "SUCCESSFUL" => Self::Successful,
            "FAILED" => Self::Failed,
            _ => Self::Unknown(value.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageVersionStatus {
    Active,
    Inactive,
    Unknown(String),
}

impl ImageVersionStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Active => "ACTIVE",
            Self::Inactive => "INACTIVE",
            Self::Unknown(value) => value,
        }
    }

    pub(super) fn from_aws(value: &str) -> Self {
        match value {
            "ACTIVE" => Self::Active,
            "INACTIVE" => Self::Inactive,
            _ => Self::Unknown(value.into()),
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedImageRelease {
    pub image_version: String,
    pub image_state: ImageState,
    pub version_state: ImageVersionState,
    pub version_status: ImageVersionStatus,
    pub state_reason: Option<String>,
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
