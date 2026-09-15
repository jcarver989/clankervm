use aws_sdk_lambdamicrovms::error::{ProvideErrorMetadata, SdkError};
use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MicroVmClientError {
    #[error("{operation} failed: {message}")]
    Service {
        operation: &'static str,
        message: String,
    },
    #[error("AWS returned an invalid application credential")]
    ApplicationToken,
    #[error("{operation} failed ({kind:?})")]
    AwsOperationFailed {
        operation: &'static str,
        kind: AwsFailure,
    },
    #[error("versions_to_keep must be at least 1")]
    InvalidVersionsToKeep,
    #[error("no log stream `{stream}` in log group `{group}`")]
    NoLogStream { group: String, stream: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AwsFailure {
    Transient,
    Auth,
    Conflict,
    Rejected,
    Unknown,
}

impl MicroVmClientError {
    pub(super) fn from_aws_error<E, R>(operation: &'static str, error: &SdkError<E, R>) -> Self
    where
        E: ProvideErrorMetadata + std::error::Error,
    {
        let kind = match error {
            SdkError::TimeoutError(_) => AwsFailure::Transient,
            SdkError::DispatchFailure(failure) if failure.is_timeout() || failure.is_io() => {
                AwsFailure::Transient
            }
            SdkError::ServiceError(_) => match error.code() {
                Some(
                    "ThrottlingException"
                    | "TooManyRequestsException"
                    | "ServiceException"
                    | "InternalServerException"
                    | "ServiceUnavailableException",
                ) => AwsFailure::Transient,
                Some(
                    "AccessDeniedException"
                    | "UnrecognizedClientException"
                    | "InvalidSignatureException",
                ) => AwsFailure::Auth,
                Some("ConflictException" | "ResourceConflictException") => AwsFailure::Conflict,
                Some(
                    "ValidationException"
                    | "ResourceNotFoundException"
                    | "ServiceQuotaExceededException"
                    | "ResourceNotReadyException",
                ) => AwsFailure::Rejected,
                _ => AwsFailure::Unknown,
            },
            _ => AwsFailure::Unknown,
        };
        Self::AwsOperationFailed { operation, kind }
    }

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
