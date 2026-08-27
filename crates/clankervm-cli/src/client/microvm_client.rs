use super::error::MicroVmClientError;
use crate::{Arn, Tags};
use async_stream::try_stream;
use futures_util::Stream;
use serde::{Deserialize, Deserializer};
use std::str::FromStr;
use std::time::Duration;
use tokio::time::sleep;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageReleasePhase {
    Pending,
    Ready,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedImageRelease {
    pub image_version: String,
    pub image_state: ImageState,
    pub version_state: ImageVersionState,
    pub version_status: ImageVersionStatus,
    pub state_reason: Option<String>,
}

impl ObservedImageRelease {
    /// Unknown or unexpected states fail fast instead of polling forever.
    pub fn phase(&self) -> ImageReleasePhase {
        let image_pending = matches!(
            self.image_state,
            ImageState::Creating | ImageState::Created | ImageState::Updating | ImageState::Updated
        );
        let version_pending = matches!(
            self.version_state,
            ImageVersionState::Pending
                | ImageVersionState::InProgress
                | ImageVersionState::Successful
        );
        let ready = matches!(self.image_state, ImageState::Created | ImageState::Updated)
            && self.version_state == ImageVersionState::Successful
            && self.version_status == ImageVersionStatus::Active;
        if ready {
            ImageReleasePhase::Ready
        } else if image_pending && version_pending {
            ImageReleasePhase::Pending
        } else {
            ImageReleasePhase::Failed
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

    fn poll_image_release(
        &self,
        request: InspectImageRequest,
        initial: Option<ObservedImageRelease>,
        interval: Duration,
    ) -> impl Stream<Item = Result<Option<ObservedImageRelease>, MicroVmClientError>> + '_ {
        try_stream! {
            let mut initial = initial;
            loop {
                let observed = match initial.take() {
                    Some(observed) => Some(observed),
                    None => self.inspect_image(request.clone()).await?,
                };

                let terminal = observed
                    .as_ref()
                    .is_some_and(|observed| observed.phase() != ImageReleasePhase::Pending);

                yield observed;

                if terminal {
                    return;
                }

                sleep(interval).await;
            }
        }
    }

    async fn prune_image_versions(
        &self,
        request: PruneImageVersionsRequest,
    ) -> Result<(), MicroVmClientError>;

    async fn run_microvm(
        &self,
        request: RunMicroVmRequest,
    ) -> Result<RunMicroVmResponse, MicroVmClientError>;
}
