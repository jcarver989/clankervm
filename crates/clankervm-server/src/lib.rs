#![doc = include_str!("../README.md")]

mod error;
mod handlers;
mod lambda_hook_server;
mod request;
mod state;
use clap::Parser;
pub use error::HookServerError;
pub use lambda_hook_server::{BASE_PATH, LambdaHookServer};
pub use request::{RunHookPayload, RunHookRequest};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "clankervm-server",
    about = "Runs a HTTP server that responds to AWS Lambda MicroVM lifecycle hooks"
)]
pub struct HookServerArgs {
    #[arg(long, env = "HOOK_SERVER_PORT", default_value = "0.0.0.0:9000")]
    pub port: SocketAddr,

    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_filter: String,
}

pub async fn run(args: HookServerArgs) -> Result<(), HookServerError> {
    let filter = EnvFilter::try_new(&args.log_filter).map_err(HookServerError::InvalidLogFilter)?;
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .finish()
        .try_init();

    let listener = TcpListener::bind(args.port)
        .await
        .map_err(HookServerError::Bind)?;

    info!(address = %listener.local_addr().map_err(HookServerError::Bind)?, "Lambda MicroVM hook server listening");
    LambdaHookServer::new().serve(listener).await
}
