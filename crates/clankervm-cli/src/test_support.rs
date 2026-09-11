//! Shared fixtures for the crate's own tests.

use crate::client::Observation;
use crate::config::ProjectConfig;
use std::fs;
use std::path::Path;

/// A `[run]` entry naming the account role tests resolve the image from.
pub(crate) const ROLE: &str = "execution-role-arn = \"arn:aws:iam::123456789012:role/run\"\n";

pub(crate) fn active(version: &str) -> Observation {
    observation(version, "CREATED", "SUCCESSFUL", "ACTIVE")
}

pub(crate) fn pending(version: &str) -> Observation {
    observation(version, "CREATING", "IN_PROGRESS", "INACTIVE")
}

pub(crate) fn failed(version: &str) -> Observation {
    observation(version, "CREATED", "FAILED", "INACTIVE")
}

fn observation(
    version: &str,
    image_state: &str,
    version_state: &str,
    version_status: &str,
) -> Observation {
    Observation {
        image_version: version.into(),
        image_state: image_state.into(),
        version_state: version_state.into(),
        version_status: version_status.into(),
        state_reason: None,
    }
}

/// Writes a real project file and loads it, so tests exercise the file schema
/// and its precedence rules instead of a hand-built config.
pub(crate) fn project(directory: &Path, sections: &str) -> ProjectConfig {
    let path = directory.join("clankervm.toml");
    fs::write(
        &path,
        format!("schema-version = 1\n[image]\nname = \"demo\"\nregion = \"us-east-1\"\n{sections}"),
    )
    .unwrap();
    ProjectConfig::load(&path, None).unwrap()
}
