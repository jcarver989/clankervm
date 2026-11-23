use aws_credential_types::provider::error::CredentialsError;
use aws_sigv4::http_request::SigningError;
use aws_sigv4::sign::v4::signing_params::BuildError;
use axum::http::header::ToStrError;
use axum::http::{Error as HttpError, StatusCode};
use axum::response::{IntoResponse, Response};
use std::io;
use tracing::error;

#[derive(Debug, thiserror::Error)]
pub enum SigV4ProxyError {
    #[error("invalid routes: {0}")]
    InvalidRoutes(#[from] serde_json::Error),
    #[error("route prefix {0:?} must start with a slash")]
    InvalidPrefix(String),
    #[error("no AWS credentials provider is configured")]
    NoCredentials,
    #[error("failed to build the HTTP client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("failed to serve the proxy: {0}")]
    Serve(#[from] io::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ForwardError {
    #[error("invalid request URL: {0}")]
    InvalidUrl(#[from] HttpError),
    #[error("a request header is not text: {0}")]
    HeaderNotText(#[from] ToStrError),
    #[error("get AWS credentials: {0}")]
    Credentials(#[from] CredentialsError),
    #[error("sign the request: {0}")]
    SigningParams(#[from] BuildError),
    #[error("sign the request: {0}")]
    Sign(#[from] SigningError),
    #[error("send the request: {0}")]
    Send(#[from] reqwest::Error),
}

impl IntoResponse for ForwardError {
    fn into_response(self) -> Response {
        let status = if matches!(self, Self::InvalidUrl(_) | Self::HeaderNotText(_)) {
            StatusCode::BAD_REQUEST
        } else {
            error!(error = %self, "request failed");
            StatusCode::BAD_GATEWAY
        };
        (status, format!("{self}\n")).into_response()
    }
}
