//! Shared fixtures for the crate's own tests.

use crate::client::{LogEvent, MicroVmDetails, MicroVmSummary, Observation};
use crate::config::ProjectConfig;
use aws_sdk_lambdamicrovms::types::{
    MicrovmImageState, MicrovmImageVersionState, MicrovmImageVersionStatus, MicrovmState,
};
use aws_smithy_types::DateTime;
use std::fs;
use std::path::Path;

/// A `[microvm.run]` entry naming the account role tests resolve the image from.
pub(crate) const ROLE: &str = "iam-role = \"arn:aws:iam::123456789012:role/run\"\n";

/// The image ARN the shared fixtures report.
pub(crate) const IMAGE_ARN: &str = "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo";

/// The endpoint a fake MicroVM's pty is reachable at.
pub(crate) const ENDPOINT: &str = "https://example.test";

/// The ingress connector that exposes a MicroVM's pty.
pub(crate) const SHELL_CONNECTOR_ARN: &str =
    "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:SHELL_INGRESS";

pub(crate) fn active(version: &str) -> Observation {
    observation(
        version,
        MicrovmImageState::Created,
        MicrovmImageVersionState::Successful,
        MicrovmImageVersionStatus::Active,
    )
}

pub(crate) fn pending(version: &str) -> Observation {
    observation(
        version,
        MicrovmImageState::Creating,
        MicrovmImageVersionState::InProgress,
        MicrovmImageVersionStatus::Inactive,
    )
}

pub(crate) fn failed(version: &str) -> Observation {
    observation(
        version,
        MicrovmImageState::Created,
        MicrovmImageVersionState::Failed,
        MicrovmImageVersionStatus::Inactive,
    )
}

fn observation(
    version: &str,
    image_state: MicrovmImageState,
    version_state: MicrovmImageVersionState,
    version_status: MicrovmImageVersionStatus,
) -> Observation {
    Observation {
        image_version: version.into(),
        image_state: Some(image_state),
        version_state,
        version_status,
        state_reason: None,
    }
}

/// Builds a [`MicroVmSummary`] with predictable defaults.
pub(crate) struct MicroVmSummaryBuilder {
    summary: MicroVmSummary,
}

impl MicroVmSummaryBuilder {
    pub(crate) fn new(microvm_id: &str) -> Self {
        Self {
            summary: MicroVmSummary {
                microvm_id: microvm_id.into(),
                state: MicrovmState::Running,
                image_arn: IMAGE_ARN.into(),
                image_version: "1".into(),
                started_at: DateTime::from_secs(0),
            },
        }
    }

    pub(crate) fn state(mut self, state: MicrovmState) -> Self {
        self.summary.state = state;
        self
    }

    pub(crate) fn image_arn(mut self, arn: &str) -> Self {
        self.summary.image_arn = arn.into();
        self
    }

    pub(crate) fn image_version(mut self, version: &str) -> Self {
        self.summary.image_version = version.into();
        self
    }

    pub(crate) fn started_at(mut self, seconds: i64) -> Self {
        self.summary.started_at = DateTime::from_secs(seconds);
        self
    }

    pub(crate) fn build(self) -> MicroVmSummary {
        self.summary
    }
}

/// Builds a [`MicroVmDetails`] with predictable defaults: a running MicroVM
/// whose pty is exposed at [`ENDPOINT`].
pub(crate) struct MicroVmDetailsBuilder {
    details: MicroVmDetails,
}

impl MicroVmDetailsBuilder {
    pub(crate) fn new(microvm_id: &str) -> Self {
        Self {
            details: MicroVmDetails {
                microvm_id: microvm_id.into(),
                state: MicrovmState::Running,
                state_reason: None,
                endpoint: ENDPOINT.into(),
                ingress_network_connectors: vec![SHELL_CONNECTOR_ARN.into()],
            },
        }
    }

    pub(crate) fn state(mut self, state: MicrovmState) -> Self {
        self.details.state = state;
        self
    }

    pub(crate) fn state_reason(mut self, reason: &str) -> Self {
        self.details.state_reason = Some(reason.into());
        self
    }

    pub(crate) fn endpoint(mut self, endpoint: &str) -> Self {
        self.details.endpoint = endpoint.into();
        self
    }

    pub(crate) fn ingress_network_connectors(
        mut self,
        connectors: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        self.details.ingress_network_connectors =
            connectors.into_iter().map(str::to_owned).collect();
        self
    }

    pub(crate) fn build(self) -> MicroVmDetails {
        self.details
    }
}

/// Builds a [`LogEvent`] with predictable defaults.
pub(crate) struct LogEventBuilder {
    event: LogEvent,
}

impl LogEventBuilder {
    pub(crate) fn new(message: &str) -> Self {
        Self {
            event: LogEvent {
                timestamp: DateTime::from_secs(0),
                message: message.into(),
            },
        }
    }

    pub(crate) fn at(mut self, seconds: i64) -> Self {
        self.event.timestamp = DateTime::from_secs(seconds);
        self
    }

    pub(crate) fn build(self) -> LogEvent {
        self.event
    }
}

/// Writes a real project file and loads it, so tests exercise the file schema
/// and its precedence rules instead of a hand-built config.
pub(crate) fn project(directory: &Path, sections: &str) -> ProjectConfig {
    let path = directory.join("clankervm.toml");
    fs::write(
        &path,
        format!("[aws]\nregion = \"us-east-1\"\n[microvm]\nname = \"demo\"\n{sections}"),
    )
    .unwrap();
    ProjectConfig::load(&path, None).unwrap()
}
