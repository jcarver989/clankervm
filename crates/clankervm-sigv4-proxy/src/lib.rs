#![doc = include_str!("../README.md")]

mod error;
mod proxy;
use clap::Parser;
pub use error::SigV4ProxyError;
pub use proxy::{Route, router};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "clankervm-sigv4-proxy",
    about = "Runs a HTTP proxy that signs requests for AWS endpoints as the MicroVM's execution role"
)]
pub struct SigV4ProxyArgs {
    /// Address to listen on.
    #[arg(long, env = "SIGV4_PROXY_PORT", default_value = "127.0.0.1:8790")]
    pub port: SocketAddr,

    /// JSON object of the endpoints to serve: each key is a path prefix, and its value an object
    /// with an `endpoint`, a `service`, a `region` and optional `headers`.
    #[arg(long, env = "SIGV4_PROXY_ROUTES")]
    pub routes: String,
}

pub async fn run(args: SigV4ProxyArgs) -> Result<(), SigV4ProxyError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    let routes: BTreeMap<String, Route> = serde_json::from_str(&args.routes)?;
    let credentials = aws_config::load_from_env()
        .await
        .credentials_provider()
        .ok_or(SigV4ProxyError::NoCredentials)?;
    let listener = TcpListener::bind(args.port).await?;
    let address = listener.local_addr()?;
    for (prefix, route) in &routes {
        info!(%address, path = prefix, endpoint = %route.endpoint, "proxying");
    }
    Ok(axum::serve(listener, router(routes, credentials)?).await?)
}
