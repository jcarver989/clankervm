use std::io;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const ROUTES: &str = r#"{
    "/": { "endpoint": "https://gw.gateway.bedrock-agentcore.us-west-2.amazonaws.com", "service": "bedrock-agentcore", "region": "us-west-2" },
    "/otel-traces/": { "endpoint": "https://xray.us-west-2.amazonaws.com", "service": "xray", "region": "us-west-2" }
}"#;

#[tokio::test]
async fn serves_the_routes_in_its_environment() -> Result<(), TestError> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_clankervm-sigv4-proxy"))
        .args(["--port", "127.0.0.1:0"])
        .env("SIGV4_PROXY_ROUTES", ROUTES)
        .env_remove("RUST_LOG")
        .env("AWS_REGION", "us-west-2")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().ok_or(TestError::Exited)?;
    let mut lines = BufReader::new(stdout).lines();

    let mut proxying = Vec::new();
    while proxying.len() < 2 {
        let line = lines.next_line().await?.ok_or(TestError::Exited)?;
        if line.contains("proxying") {
            proxying.push(line);
        }
    }

    assert!(proxying[0].contains("path=\"/\""), "got {proxying:?}");
    assert!(
        proxying[1].contains("path=\"/otel-traces/\"")
            && proxying[1].contains("endpoint=https://xray.us-west-2.amazonaws.com/"),
        "got {proxying:?}"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("the proxy exited before it served its routes")]
    Exited,
}
