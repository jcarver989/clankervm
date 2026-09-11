use crate::handlers::{not_found, ready, run, terminate, validate};
use crate::state::HookServerState;
use crate::{HookServerError, RunHookPayload};
use axum::routing::post;
use axum::{Router, serve};
use std::future::{Future, pending};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::select;

pub const BASE_PATH: &str = "/aws/lambda-microvms/runtime/v1";
pub const DEFAULT_TERMINATE_GRACE_PERIOD: Duration = Duration::from_secs(20);

/// HTTP server for AWS Lambda MicroVM lifecycle hooks.
///
/// See: <https://docs.aws.amazon.com/lambda/latest/dg/microvms-launching.html#microvms-launching-lifecycle-hooks>
pub struct LambdaHookServer {
    state: Arc<HookServerState>,
    ready_command: Option<RunHookPayload>,
    validate_command: Option<RunHookPayload>,
}

impl LambdaHookServer {
    pub fn new() -> Self {
        Self::with_terminate_grace_period(DEFAULT_TERMINATE_GRACE_PERIOD)
    }

    pub fn with_terminate_grace_period(terminate_grace_period: Duration) -> Self {
        Self {
            state: HookServerState::new(terminate_grace_period),
            ready_command: None,
            validate_command: None,
        }
    }

    /// Runs initialization once before readiness can succeed, without consuming the run command.
    pub fn with_ready_command(mut self, command: RunHookPayload) -> Self {
        self.ready_command = Some(command);
        self
    }

    /// Runs validation once on the first validate hook, after snapshot restoration.
    pub fn with_validate_command(mut self, command: RunHookPayload) -> Self {
        self.validate_command = Some(command);
        self
    }

    pub async fn serve(self, listener: TcpListener) -> Result<(), HookServerError> {
        self.serve_with_shutdown(listener, pending()).await
    }

    pub async fn serve_with_shutdown(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), HookServerError> {
        if let Some(command) = self.ready_command {
            self.state.initialize(command)?;
        }
        let state = Arc::clone(&self.state);
        let mut router = Router::new();
        if let Some(command) = self.validate_command {
            self.state.configure_validation(command);
            router = router.route(&format!("{BASE_PATH}/validate"), post(validate));
        }
        let router = router
            .route(&format!("{BASE_PATH}/ready"), post(ready))
            .route(&format!("{BASE_PATH}/run"), post(run))
            .route(&format!("{BASE_PATH}/terminate"), post(terminate))
            .fallback(not_found)
            .method_not_allowed_fallback(not_found)
            .with_state(Arc::clone(&self.state));

        serve(listener, router)
            .with_graceful_shutdown(async move {
                select! {
                    () = state.wait_for_completion() => {}
                    () = shutdown => {
                        state.begin_shutdown();
                        state.wait_for_completion().await;
                    }
                }
            })
            .await
            .map_err(HookServerError::Server)?;

        self.state.take_result()
    }
}

impl Default for LambdaHookServer {
    fn default() -> Self {
        Self::new()
    }
}
