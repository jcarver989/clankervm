use crate::client::ObservedImageRelease;
use crate::commands::{PushConfig, RunConfig, StatusConfig};
use crate::config::{ImageConfig, Project, ProjectConfig};
use std::path::{Path, PathBuf};

pub(crate) struct ProjectConfigBuilder {
    name: String,
    region: String,
    push: PushConfig,
    status: StatusConfig,
    run: RunConfig,
}

impl ProjectConfigBuilder {
    pub(crate) fn new() -> Self {
        Self {
            name: "demo".into(),
            region: "us-east-1".into(),
            push: PushConfig::default(),
            status: StatusConfig::default(),
            run: RunConfig::default(),
        }
    }

    pub(crate) fn push(mut self, push: PushConfig) -> Self {
        self.push = push;
        self
    }

    pub(crate) fn run(mut self, run: RunConfig) -> Self {
        self.run = run;
        self
    }

    pub(crate) fn build(self) -> ProjectConfig {
        ProjectConfig {
            schema_version: 1,
            image: ImageConfig {
                name: self.name,
                region: self.region,
            },
            push: self.push,
            status: self.status,
            run: self.run,
        }
    }
}

pub(crate) struct ProjectBuilder {
    root: PathBuf,
    config: ProjectConfigBuilder,
}

impl ProjectBuilder {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.into(),
            config: ProjectConfigBuilder::new(),
        }
    }

    pub(crate) fn push(mut self, push: PushConfig) -> Self {
        self.config = self.config.push(push);
        self
    }

    pub(crate) fn build(self) -> Project {
        Project::from_parts(self.config.build(), self.root)
    }
}

pub(crate) struct ObservedImageReleaseBuilder {
    release: ObservedImageRelease,
}

impl ObservedImageReleaseBuilder {
    pub(crate) fn active(version: &str) -> ObservedImageRelease {
        Self::new(version, "CREATED", "SUCCESSFUL", "ACTIVE").build()
    }

    pub(crate) fn pending(version: &str) -> ObservedImageRelease {
        Self::new(version, "CREATING", "IN_PROGRESS", "INACTIVE").build()
    }

    pub(crate) fn failed(version: &str) -> Self {
        Self::new(version, "CREATED", "FAILED", "INACTIVE")
    }

    pub(crate) fn reason(mut self, reason: &str) -> Self {
        self.release.state_reason = Some(reason.into());
        self
    }

    pub(crate) fn build(self) -> ObservedImageRelease {
        self.release
    }

    fn new(version: &str, image_state: &str, version_state: &str, version_status: &str) -> Self {
        Self {
            release: ObservedImageRelease {
                image_version: version.into(),
                image_state: image_state.into(),
                version_state: version_state.into(),
                version_status: version_status.into(),
                state_reason: None,
            },
        }
    }
}
