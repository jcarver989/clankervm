use crate::error::{ApiError, error_response};
use crate::request::{RunHookPayload, RunHookRequest};
use crate::state::{HookServerState, Lifecycle};
use axum::extract::{Json, State, rejection::JsonRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub(crate) struct StatusBody {
    status: &'static str,
}

pub(crate) async fn ready(State(state): State<Arc<HookServerState>>) -> impl IntoResponse {
    if state.is_ready() {
        (StatusCode::OK, Json(StatusBody { status: "ready" }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(StatusBody {
                status: "initializing",
            }),
        )
    }
}

pub(crate) async fn validate(
    State(state): State<Arc<HookServerState>>,
) -> Result<Response, ApiError> {
    if state.validate()? {
        Ok((
            StatusCode::OK,
            Json(StatusBody {
                status: "validated",
            }),
        )
            .into_response())
    } else {
        Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(StatusBody {
                status: "validating",
            }),
        )
            .into_response())
    }
}

pub(crate) async fn run(
    State(state): State<Arc<HookServerState>>,
    request: Result<Json<RunHookRequest>, JsonRejection>,
) -> Result<Json<StatusBody>, ApiError> {
    let Json(request) = request.map_err(|_| ApiError::bad_request("invalid request JSON"))?;

    if request.microvm_id.trim().is_empty() {
        return Err(ApiError::bad_request("microvmId must not be empty"));
    }

    if !state.is_ready() {
        return Err(ApiError::not_ready());
    }

    if !state.claim_run() {
        return Err(ApiError::conflict("run already started"));
    }

    state.start_run(request.microvm_id, request.run_hook_payload)?;
    Ok(Json(StatusBody { status: "accepted" }))
}

pub(crate) async fn restart(
    State(state): State<Arc<HookServerState>>,
    request: Result<Json<RunHookPayload>, JsonRejection>,
) -> Result<Json<StatusBody>, ApiError> {
    let Json(command) = request.map_err(|_| ApiError::bad_request("invalid request JSON"))?;
    state.lifecycle(Lifecycle::Restart(command)).await?;
    Ok(Json(StatusBody {
        status: "restarted",
    }))
}

pub(crate) async fn suspend(
    State(state): State<Arc<HookServerState>>,
) -> Result<Json<StatusBody>, ApiError> {
    state.lifecycle(Lifecycle::Suspend).await?;
    Ok(Json(StatusBody {
        status: "suspended",
    }))
}

pub(crate) async fn resume(
    State(state): State<Arc<HookServerState>>,
) -> Result<Json<StatusBody>, ApiError> {
    state.lifecycle(Lifecycle::Resume).await?;
    Ok(Json(StatusBody { status: "resumed" }))
}

pub(crate) async fn terminate(State(state): State<Arc<HookServerState>>) -> Json<StatusBody> {
    state.begin_shutdown();
    state.wait_for_completion().await;
    Json(StatusBody {
        status: "terminating",
    })
}

pub(crate) async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}
