use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MicroVmClientError {
    #[error("{operation} failed: {message}")]
    Service {
        operation: &'static str,
        message: String,
    },
    #[error("versions_to_keep must be at least 1")]
    InvalidVersionsToKeep,
}

impl MicroVmClientError {
    pub(super) fn service(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Service {
            operation,
            message: error.to_string(),
        }
    }
}
