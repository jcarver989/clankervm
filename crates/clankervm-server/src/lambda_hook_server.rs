use crate::HookServerError;
use crate::handlers::{not_found, ready, run, terminate};
use crate::state::HookServerState;
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
}

impl LambdaHookServer {
    pub fn new() -> Self {
        Self::with_terminate_grace_period(DEFAULT_TERMINATE_GRACE_PERIOD)
    }

    pub fn with_terminate_grace_period(terminate_grace_period: Duration) -> Self {
        Self {
            state: HookServerState::new(terminate_grace_period),
        }
    }

    pub async fn serve(self, listener: TcpListener) -> Result<(), HookServerError> {
        self.serve_with_shutdown(listener, pending()).await
    }

    pub async fn serve_with_shutdown(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), HookServerError> {
        let state = Arc::clone(&self.state);
        let router = Router::new()
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
