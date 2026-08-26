#![doc = include_str!("../README.md")]

mod command;
mod error;
mod handlers;
mod lambda_hook_server;
mod request;
mod state;
use clap::Parser;
pub use error::HookServerError;
pub use lambda_hook_server::{BASE_PATH, DEFAULT_TERMINATE_GRACE_PERIOD, LambdaHookServer};
pub use request::{RunHookPayload, RunHookRequest};
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::select;
use tokio::signal::unix::{SignalKind, signal};
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

    /// Seconds to wait after SIGTERM before killing the run command.
    #[arg(
        long,
        env = "HOOK_SERVER_TERMINATE_GRACE_PERIOD",
        default_value_t = DEFAULT_TERMINATE_GRACE_PERIOD.as_secs(),
        value_parser = clap::value_parser!(u64).range(0..=3_600)
    )]
    pub terminate_grace_period: u64,
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
    let shutdown = shutdown_signal()?;
    LambdaHookServer::with_terminate_grace_period(Duration::from_secs(args.terminate_grace_period))
        .serve_with_shutdown(listener, shutdown)
        .await
}

fn shutdown_signal() -> Result<impl Future<Output = ()> + Send + 'static, HookServerError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(HookServerError::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(HookServerError::Signal)?;
    Ok(async move {
        select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
}
