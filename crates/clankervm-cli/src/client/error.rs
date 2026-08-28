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
    pub(super) fn service(
        operation: &'static str,
        error: impl std::error::Error + 'static,
    ) -> Self {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            let cause_message = cause.to_string();
            if cause_message != "service error" {
                message = cause_message;
            }
            source = cause.source();
        }
        Self::Service { operation, message }
    }
}
