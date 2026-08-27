use crate::release::ReleaseStatus;
use crate::{ClankerError, OutputFormat};
use serde::Serialize;

pub(crate) fn render<T: Serialize>(
    format: OutputFormat,
    value: &T,
    human: impl FnOnce() -> String,
) -> Result<(), ClankerError> {
    match format {
        OutputFormat::Human => println!("{}", human()),
        OutputFormat::Json => println!("{}", serde_json::to_string(value)?),
    }
    Ok(())
}

pub(crate) struct ReleaseProgress {
    enabled: bool,
    prior: Option<(String, String, String)>,
}

impl ReleaseProgress {
    pub(crate) fn new(format: OutputFormat) -> Self {
        Self {
            enabled: matches!(format, OutputFormat::Human),
            prior: None,
        }
    }

    pub(crate) fn report(&mut self, release: &ReleaseStatus) {
        if !self.enabled {
            return;
        }
        let state = (
            release.image_state.clone(),
            release.version_state.clone(),
            release.version_status.clone(),
        );
        if self.prior.as_ref() == Some(&state) {
            return;
        }
        eprintln!(
            "  Image: {:<10} Build: {:<10} Activation: {}",
            release.image_state, release.version_state, release.version_status
        );
        self.prior = Some(state);
    }
}
