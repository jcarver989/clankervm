use crate::client::Observation;
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

/// Human progress on stderr, suppressed once the same state was reported.
pub(crate) struct ReleaseProgress {
    enabled: bool,
    prior: Option<Observation>,
}

impl ReleaseProgress {
    pub(crate) fn new(format: OutputFormat) -> Self {
        Self {
            enabled: matches!(format, OutputFormat::Human),
            prior: None,
        }
    }

    pub(crate) fn report(&mut self, release: &ReleaseStatus) {
        let observed = &release.observation;
        if !self.enabled || self.prior.as_ref() == Some(observed) {
            return;
        }
        eprintln!(
            "  Image: {:<10} Build: {:<10} Activation: {}",
            observed.image_state, observed.version_state, observed.version_status
        );
        self.prior = Some(observed.clone());
    }
}
