use crate::HookServerError;
use crate::handlers::{not_found, ready, run, terminate};
use crate::state::HookServerState;
use axum::routing::post;
use axum::{Router, serve};
use std::sync::Arc;
use tokio::net::TcpListener;

pub const BASE_PATH: &str = "/aws/lambda-microvms/runtime/v1";

/// HTTP server for AWS Lambda MicroVM lifeycle hooks
///
/// See: https://docs.aws.amazon.com/lambda/latest/dg/microvms-launching.html#microvms-launching-lifecycle-hooks
pub struct LambdaHookServer {
    state: Arc<HookServerState>,
}

impl LambdaHookServer {
    pub fn new() -> Self {
        Self {
            state: HookServerState::new(),
        }
    }

    pub async fn serve(self, listener: TcpListener) -> Result<(), HookServerError> {
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
                state.wait_for_completion().await;
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
