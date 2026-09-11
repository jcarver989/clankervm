use super::error::MicroVmClientError;
use crate::arn::Arn;
use crate::artifact::Artifact;
use aws_sdk_lambdamicrovms::types::Capability;
use serde::Serialize;
use std::collections::BTreeMap;

/// AWS image states that are still making progress.
const IMAGE_PENDING_STATES: [&str; 4] = ["CREATING", "CREATED", "UPDATING", "UPDATED"];
/// AWS image version states that are still making progress.
const VERSION_PENDING_STATES: [&str; 3] = ["PENDING", "IN_PROGRESS", "SUCCESSFUL"];
/// AWS image states a ready release can report.
const IMAGE_READY_STATES: [&str; 2] = ["CREATED", "UPDATED"];
/// AWS image version state of a successfully built release.
const VERSION_READY_STATE: &str = "SUCCESSFUL";
/// AWS image version status of an active release.
pub(crate) const VERSION_ACTIVE_STATUS: &str = "ACTIVE";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageHooks {
    pub port: i32,
    pub ready_timeout_seconds: i32,
    pub run_timeout_seconds: i32,
    pub terminate_timeout_seconds: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageConfiguration {
    pub base_image_arn: Arn,
    pub build_role_arn: Arn,
    pub description: String,
    pub minimum_memory_mib: Option<i32>,
    pub capabilities: Vec<Capability>,
    pub egress_network_connector: Arn,
    pub hooks: ImageHooks,
}

/// Everything the client needs to publish one bundle as an image version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageSpec {
    pub arn: Arn,
    pub name: String,
    pub bucket: String,
    pub tags: BTreeMap<String, String>,
    pub configuration: ImageConfiguration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Published {
    pub version: String,
    pub artifact_uri: String,
}

/// What AWS currently reports for one image release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Observation {
    pub image_version: String,
    pub image_state: String,
    pub version_state: String,
    pub version_status: String,
    pub state_reason: Option<String>,
}

impl Observation {
    /// The image is built and this version is active.
    pub(crate) fn is_ready(&self) -> bool {
        IMAGE_READY_STATES.contains(&self.image_state.as_str())
            && self.version_state == VERSION_READY_STATE
            && self.version_status == VERSION_ACTIVE_STATUS
    }

    /// Still building; unknown or unexpected states count as failures so
    /// polling fails fast instead of waiting forever.
    pub(crate) fn is_pending(&self) -> bool {
        IMAGE_PENDING_STATES.contains(&self.image_state.as_str())
            && VERSION_PENDING_STATES.contains(&self.version_state.as_str())
    }
}

/// A release AWS has not reported yet.
impl Default for Observation {
    fn default() -> Self {
        Self {
            image_version: String::new(),
            image_state: "PENDING".into(),
            version_state: "PENDING".into(),
            version_status: "INACTIVE".into(),
            state_reason: None,
        }
    }
}

/// Everything the client needs to start one MicroVM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchSpec {
    pub image_arn: Arn,
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
pub(crate) struct Launch {
    pub microvm_id: String,
    pub image_version: String,
}

/// The AWS operations ClankerVM performs, expressed in domain terms.
#[allow(async_fn_in_trait)]
pub(crate) trait MicroVmClient: Send + Sync {
    /// Creates or updates the image and returns the version AWS reports.
    async fn publish(
        &self,
        spec: &ImageSpec,
        bundle: &Artifact,
    ) -> Result<Published, MicroVmClientError>;

    /// The current release state, or `None` when the image or version is gone.
    async fn observe(
        &self,
        image: &Arn,
        version: Option<&str>,
    ) -> Result<Option<Observation>, MicroVmClientError>;

    /// Deletes inactive versions beyond the newest `keep`.
    async fn prune(&self, image: &Arn, keep: usize) -> Result<(), MicroVmClientError>;

    async fn launch(&self, spec: &LaunchSpec) -> Result<Launch, MicroVmClientError>;
}

/// The content-addressed object key of a published bundle.
pub(crate) fn artifact_key(name: &str, digest: &str) -> String {
    format!("clankervm/{name}/bundles/{digest}.zip")
}
