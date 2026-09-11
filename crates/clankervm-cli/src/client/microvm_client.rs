use super::error::MicroVmClientError;
use crate::ClankerError;
use crate::arn::Arn;
use crate::artifact::Artifact;
use aws_sdk_lambdamicrovms::types::{
    Capability, Hooks, MicrovmImageState, MicrovmImageVersionState, MicrovmImageVersionStatus,
    MicrovmState, Resources,
};
use aws_smithy_types::DateTime;
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};

/// The ingress connector that makes AWS expose a MicroVM's pty.
pub(crate) const SHELL_INGRESS: &str = "SHELL_INGRESS";

/// Everything AWS needs to build one image version, in AWS terms.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImageConfiguration {
    pub base_image_arn: Arn,
    pub build_role_arn: Arn,
    pub description: String,
    pub resources: Option<Vec<Resources>>,
    pub capabilities: Vec<Capability>,
    pub egress_network_connector: Arn,
    pub hooks: Hooks,
    pub environment_variables: HashMap<String, String>,
}

/// Everything the client needs to publish one bundle as an image version.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImageSpec {
    pub arn: Arn,
    pub name: String,
    pub bucket: String,
    /// The S3 key prefix bundles are uploaded under, without a trailing slash.
    pub artifact_prefix: String,
    pub tags: BTreeMap<String, String>,
    pub configuration: ImageConfiguration,
}

impl ImageSpec {
    /// The content-addressed object key of a published bundle.
    ///
    /// The prefix is configurable because an image's build role is often scoped
    /// to a key prefix that predates ClankerVM.
    pub(crate) fn artifact_key(&self, digest: &str) -> String {
        format!(
            "{}/{}/bundles/{digest}.zip",
            self.artifact_prefix, self.name
        )
    }
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
    /// Unset until AWS reports an image state; there is no placeholder state.
    #[serde(serialize_with = "serialize_optional_state")]
    pub image_state: Option<MicrovmImageState>,
    #[serde(serialize_with = "serialize_state")]
    pub version_state: MicrovmImageVersionState,
    #[serde(serialize_with = "serialize_state")]
    pub version_status: MicrovmImageVersionStatus,
    pub state_reason: Option<String>,
}

impl Observation {
    /// The image is built and this version is active.
    pub(crate) fn is_ready(&self) -> bool {
        matches!(
            self.image_state,
            Some(MicrovmImageState::Created | MicrovmImageState::Updated)
        ) && self.version_state == MicrovmImageVersionState::Successful
            && self.version_status == MicrovmImageVersionStatus::Active
    }

    /// Still building; unknown or unexpected states count as failures so
    /// polling fails fast instead of waiting forever.
    pub(crate) fn is_pending(&self) -> bool {
        matches!(
            self.image_state,
            Some(
                MicrovmImageState::Creating
                    | MicrovmImageState::Created
                    | MicrovmImageState::Updating
                    | MicrovmImageState::Updated
            )
        ) && matches!(
            self.version_state,
            MicrovmImageVersionState::Pending
                | MicrovmImageVersionState::InProgress
                | MicrovmImageVersionState::Successful
        )
    }

    /// The image state to show, or `UNKNOWN` before AWS reports one.
    pub(crate) fn image_state_name(&self) -> &str {
        self.image_state
            .as_ref()
            .map_or("UNKNOWN", MicrovmImageState::as_str)
    }
}

/// A release AWS has not reported yet.
impl Default for Observation {
    fn default() -> Self {
        Self {
            image_version: String::new(),
            image_state: None,
            version_state: MicrovmImageVersionState::Pending,
            version_status: MicrovmImageVersionStatus::Inactive,
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

/// One page of events read from one log stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LogPage {
    pub events: Vec<LogEvent>,
    /// Continues a forward read, and is unset when AWS reports no next page.
    pub next_token: Option<String>,
}

/// One event read from a log stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogEvent {
    #[serde(serialize_with = "serialize_timestamp")]
    pub timestamp: DateTime,
    pub message: String,
}

/// Where a read of a stream starts and which way it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LogWindow {
    /// The newest events in the stream, read backwards from its end.
    Newest,
    /// Every event in the stream, read forwards from the oldest one.
    Everything,
    /// Every event at or after this instant, read forwards.
    Since(DateTime),
}

impl LogWindow {
    /// Whether events are read from the oldest one onwards, which is the only
    /// direction that can be continued with a token.
    pub(crate) fn is_forward(&self) -> bool {
        !matches!(self, Self::Newest)
    }

    /// The instant the window starts at, in the milliseconds AWS expects.
    pub(crate) fn start_time(&self) -> Option<i64> {
        match self {
            Self::Newest | Self::Everything => None,
            Self::Since(start) => Some(start.to_millis().unwrap_or_default()),
        }
    }
}

/// Everything one read of one log stream needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogQuery {
    pub group: String,
    pub stream: String,
    pub window: LogWindow,
    /// Events requested per page; AWS caps a page at 10,000 events.
    pub limit: usize,
    /// Continues a previous forward read, which makes AWS ignore the window.
    pub next_token: Option<String>,
}

/// One MicroVM, as AWS discovery summarizes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MicroVmSummary {
    pub microvm_id: String,
    #[serde(serialize_with = "serialize_state")]
    pub state: MicrovmState,
    pub image_arn: String,
    pub image_version: String,
    #[serde(serialize_with = "serialize_timestamp")]
    pub started_at: DateTime,
}

/// RFC3339 UTC, for example `2026-08-25T00:00:00Z`.
fn serialize_timestamp<S: Serializer>(value: &DateTime, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// Writes a string-backed AWS enum as the value AWS uses on the wire.
fn serialize_state<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: AsRef<str>,
{
    serializer.serialize_str(value.as_ref())
}

/// Writes a not-yet-reported state as `null` instead of inventing one.
///
/// Serde hands `serialize_with` a reference to the field, hence `&Option<T>`.
#[allow(clippy::ref_option)]
fn serialize_optional_state<S, T>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: AsRef<str>,
{
    match value {
        Some(state) => serializer.serialize_str(state.as_ref()),
        None => serializer.serialize_none(),
    }
}

pub(crate) type MicroVmPage = (Vec<MicroVmSummary>, Option<String>);

/// One MicroVM as `GetMicrovm` describes it, in the terms `shell` needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MicroVmDetails {
    pub microvm_id: String,
    pub state: MicrovmState,
    pub state_reason: Option<String>,
    /// The host the MicroVM's pty is reachable at.
    pub endpoint: String,
    pub ingress_network_connectors: Vec<String>,
}

/// The handshake headers a MicroVM's `/shell` endpoint authenticates with.
///
/// Deliberately not `Debug`: the headers carry a bearer token that must never
/// reach a log line or an error message.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ShellToken {
    pub headers: HashMap<String, String>,
}

/// An image name or ARN accepted by AWS `image_identifier`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageIdentifier {
    Name(String),
    Arn(Arn),
}

impl ImageIdentifier {
    /// A bare name passes through; an explicit ARN must address `region`.
    pub(crate) fn parse(value: &str, region: &str) -> Result<Self, ClankerError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ClankerError::InvalidConfig(
                "image filter cannot be empty".into(),
            ));
        }
        if !value.starts_with("arn:") {
            return Ok(Self::Name(value.to_owned()));
        }
        let arn = Arn::parse(value)?;
        if arn.region() != Some(region) {
            return Err(ClankerError::InvalidConfig(format!(
                "image ARN `{value}` is not in region `{region}`"
            )));
        }
        Ok(Self::Arn(arn))
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Arn(arn) => arn.as_str(),
        }
    }
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

    /// One page of MicroVMs and the token of the page after it.
    async fn list_microvms(
        &self,
        image: Option<&ImageIdentifier>,
        version: Option<&str>,
        next_token: Option<&str>,
    ) -> Result<MicroVmPage, MicroVmClientError>;

    async fn launch(&self, spec: &LaunchSpec) -> Result<Launch, MicroVmClientError>;

    /// The names of the streams one log group holds.
    async fn log_streams(&self, group: &str) -> Result<Vec<String>, MicroVmClientError>;

    /// One page of events from one stream, oldest first.
    async fn log_events(&self, query: &LogQuery) -> Result<LogPage, MicroVmClientError>;

    /// The current description of one MicroVM, or `None` when it is gone.
    async fn describe(
        &self,
        microvm_id: &str,
    ) -> Result<Option<MicroVmDetails>, MicroVmClientError>;

    /// A token that authenticates a `/shell` handshake.
    async fn shell_token(&self, microvm_id: &str) -> Result<ShellToken, MicroVmClientError>;

    /// Stops one MicroVM; a MicroVM that is already gone is not an error.
    async fn terminate(&self, microvm_id: &str) -> Result<(), MicroVmClientError>;
}
