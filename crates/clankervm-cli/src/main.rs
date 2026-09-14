use clankervm::{ClankerError, Cli, execute};
use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match Box::pin(execute(Cli::parse())).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(ClankerError::ClientExit(code)) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
