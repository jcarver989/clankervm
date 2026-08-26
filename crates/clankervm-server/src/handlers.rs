use crate::HookServerError;
use crate::error::{ApiError, error_response};
use crate::request::RunHookRequest;
use crate::state::HookServerState;
use axum::extract::{Json, State, rejection::JsonRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Serialize;
use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::process::CommandExt;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::select;
use tokio_util::sync::CancellationToken;

pub(crate) type CommandResult = Pin<Box<dyn Future<Output = Result<(), HookServerError>> + Send>>;

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
    let Json(request) =
        request.map_err(|error| ApiError::bad_request(format!("invalid request JSON: {error}")))?;

    if request.microvm_id.trim().is_empty() {
        return Err(ApiError::bad_request("microvmId must not be empty"));
    }

    if !state.claim_run() {
        return Err(ApiError::conflict("run already started"));
    }

    let (command, args, mut environment) = request.run_hook_payload.into_parts();
    environment.insert("AWS_LAMBDA_MICROVM_ID".to_string(), request.microvm_id);
    let command_result = spawn_command(command, args, environment, state.cancellation_token())
        .map_err(|error| ApiError::initialization(&state, error))?;

    state.track_command(command_result);
    Ok(Json(StatusBody { status: "accepted" }))
}

pub async fn terminate(State(state): State<Arc<HookServerState>>) -> Json<StatusBody> {
    state.cancel();
    if state.claim_run() {
        state.finish(Ok(()));
    }
    state.wait_for_completion().await;
    Json(StatusBody {
        status: "terminating",
    })
}

pub async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}

fn spawn_command(
    command: String,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    cancellation: CancellationToken,
) -> Result<CommandResult, HookServerError> {
    let child = {
        let mut process = Command::new(command);
        process
            .args(args)
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        process.as_std_mut().process_group(0);
        process.spawn().map_err(HookServerError::CommandSpawn)?
    };

    let process_group = child.id().map(|id| Pid::from_raw(id.cast_signed()));
    Ok(Box::pin(await_command_with_cancellation(
        child,
        process_group,
        cancellation,
    )))
}

async fn await_command_with_cancellation(
    mut child: Child,
    process_group: Option<Pid>,
    cancellation: CancellationToken,
) -> Result<(), HookServerError> {
    select! {
        status = child.wait() => {
            let status = status.map_err(HookServerError::CommandWait)?;
            if status.success() { Ok(()) } else { Err(HookServerError::CommandFailed) }
        }

        () = cancellation.cancelled() => {
            if let Some(process_group) = process_group {
                let _ = killpg(process_group, Signal::SIGKILL);
            } else {
                let _ = child.kill().await;
            }

            child.wait().await.map_err(HookServerError::CommandWait)?;
            Ok(())
        }
    }
}
