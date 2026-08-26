use crate::state::HookServerState;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum HookServerError {
    #[error("failed to bind hook server: {0}")]
    Bind(#[source] io::Error),
    #[error("hook server failed: {0}")]
    Server(#[source] io::Error),
    #[error("invalid log filter: {0}")]
    InvalidLogFilter(#[source] tracing_subscriber::filter::ParseError),
    #[error("failed to start run command: {0}")]
    CommandSpawn(#[source] io::Error),
    #[error("failed while waiting for run command: {0}")]
    CommandWait(#[source] io::Error),
    #[error("run command reported failure")]
    CommandFailed,
}

pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn initialization(state: &HookServerState, error: HookServerError) -> Self {
        let message = error.to_string();
        state.finish(Err(error));
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message)
    }
}

pub(crate) fn error_response(status: StatusCode, message: &str) -> Response {
    let body: Value = json!({ "error": message });
    (status, Json(body)).into_response()
}
