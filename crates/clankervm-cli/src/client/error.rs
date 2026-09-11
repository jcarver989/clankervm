use aws_sdk_lambdamicrovms::error::{ProvideErrorMetadata, SdkError};
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
    /// Summarizes an SDK failure from the error metadata AWS returns, which is
    /// the only part of an SDK error worth showing a user.
    pub(super) fn service<E, R>(operation: &'static str, error: &SdkError<E, R>) -> Self
    where
        E: ProvideErrorMetadata + std::error::Error,
    {
        let message = match (error.code(), error.message()) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (Some(metadata), None) | (None, Some(metadata)) => metadata.to_owned(),
            (None, None) => error
                .as_service_error()
                .map_or_else(|| error.to_string(), ToString::to_string),
        };
        Self::Service { operation, message }
    }
}
