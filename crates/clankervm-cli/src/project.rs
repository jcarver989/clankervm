use std::env::current_dir;
use std::path::{Path, PathBuf};

use crate::ClankerError;
use crate::config::ProjectConfig;

#[derive(Clone, Debug)]
pub struct Project {
    config: ProjectConfig,
    root: PathBuf,
}

impl Project {
    pub fn load(config_path: &Path, region: Option<String>) -> Result<Self, ClankerError> {
        let config_path = if config_path.is_absolute() {
            config_path.to_owned()
        } else {
            current_dir()
                .map_err(|source| ClankerError::Io {
                    action: "resolve current directory".into(),
                    source,
                })?
                .join(config_path)
        };
        let mut config = ProjectConfig::load(&config_path)?;
        if let Some(region) = region {
            config.app.region = region;
        }
        let root = project_root(&config_path).to_owned();
        Ok(Self { config, root })
    }

    pub fn config(&self) -> &ProjectConfig {
        &self.config
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        }
    }
}

pub(crate) fn project_root(config_path: &Path) -> &Path {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_of_bare_config_file_is_the_current_directory() {
        assert_eq!(project_root(Path::new("clankervm.toml")), Path::new("."));
        assert_eq!(
            project_root(Path::new("project/clankervm.toml")),
            Path::new("project")
        );
    }
}
