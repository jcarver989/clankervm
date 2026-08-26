use crate::command::Command;
use crate::error::{ApiError, error_response};
use crate::request::RunHookRequest;
use crate::state::HookServerState;
use axum::extract::{Json, State, rejection::JsonRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct StatusBody {
    status: &'static str,
}

pub async fn ready() -> impl IntoResponse {
    (StatusCode::OK, Json(StatusBody { status: "ready" }))
}

pub async fn run(
    State(state): State<Arc<HookServerState>>,
    request: Result<Json<RunHookRequest>, JsonRejection>,
) -> Result<Json<StatusBody>, ApiError> {
    let Json(request) = request.map_err(|_| ApiError::bad_request("invalid request JSON"))?;

    if request.microvm_id.trim().is_empty() {
        return Err(ApiError::bad_request("microvmId must not be empty"));
    }

    if !state.claim_run() {
        return Err(ApiError::conflict("run already started"));
    }

    let (command, args, mut environment) = request.run_hook_payload.into_parts();
    environment.insert("AWS_LAMBDA_MICROVM_ID".to_string(), request.microvm_id);
    let command = Command::spawn(command, args, environment, state.terminate_grace_period())
        .map_err(|error| ApiError::initialization(&state, error))?;

    state.track_command(command.wait(state.cancellation_token()));
    Ok(Json(StatusBody { status: "accepted" }))
}

pub async fn terminate(State(state): State<Arc<HookServerState>>) -> Json<StatusBody> {
    state.begin_shutdown();
    state.wait_for_completion().await;
    Json(StatusBody {
        status: "terminating",
    })
}

pub async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}
