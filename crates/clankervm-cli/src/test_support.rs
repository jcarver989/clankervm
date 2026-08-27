use crate::client::{ImageState, ImageVersionState, ImageVersionStatus, ObservedImageRelease};
use crate::commands::{PushConfig, RunConfig, StatusConfig};
use crate::config::{AppConfig, ImageConfig, Project, ProjectConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) struct ProjectConfigBuilder {
    name: String,
    region: String,
    push: PushConfig,
    status: StatusConfig,
    run: RunConfig,
    images: BTreeMap<String, ImageConfig>,
}

impl ProjectConfigBuilder {
    pub(crate) fn new() -> Self {
        Self {
            name: "demo".into(),
            region: "us-east-1".into(),
            push: PushConfig::default(),
            status: StatusConfig::default(),
            run: RunConfig::default(),
            images: BTreeMap::new(),
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

    pub(crate) fn image(mut self, name: &str, image: ImageConfig) -> Self {
        self.images.insert(name.into(), image);
        self
    }

    pub(crate) fn build(self) -> ProjectConfig {
        ProjectConfig {
            schema_version: 1,
            app: AppConfig {
                name: self.name,
                region: self.region,
            },
            push: self.push,
            status: self.status,
            run: self.run,
            image: self.images,
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
        Self::new(
            version,
            ImageState::Created,
            ImageVersionState::Successful,
            ImageVersionStatus::Active,
        )
        .build()
    }

    pub(crate) fn pending(version: &str) -> ObservedImageRelease {
        Self::new(
            version,
            ImageState::Creating,
            ImageVersionState::InProgress,
            ImageVersionStatus::Inactive,
        )
        .build()
    }

    pub(crate) fn failed(version: &str) -> Self {
        Self::new(
            version,
            ImageState::Created,
            ImageVersionState::Failed,
            ImageVersionStatus::Inactive,
        )
    }

    pub(crate) fn reason(mut self, reason: &str) -> Self {
        self.release.state_reason = Some(reason.into());
        self
    }

    pub(crate) fn build(self) -> ObservedImageRelease {
        self.release
    }

    fn new(
        version: &str,
        image_state: ImageState,
        version_state: ImageVersionState,
        version_status: ImageVersionStatus,
    ) -> Self {
        Self {
            release: ObservedImageRelease {
                image_version: version.into(),
                image_state,
                version_state,
                version_status,
                state_reason: None,
            },
        }
    }
}
