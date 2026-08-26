mod support;

use nix::sys::signal::{Signal, kill};
use reqwest::StatusCode;
use serde_json::json;
use std::time::Duration;
use support::{RunScriptBuilder, TestServerBuilder, wait_until_gone};

#[tokio::test]
async fn sigterm_gracefully_stops_the_run_process_group_without_logging_secrets() {
    const SECRET: &str = "github-token-that-must-not-leak";

    let script = RunScriptBuilder::new("run-command").build();
    let server = TestServerBuilder::new()
        .terminate_grace_period(Duration::from_secs(2))
        .start_process();
    server.wait_until_ready().await;

    let mut payload = script.payload();
    payload["environment"]["GITHUB_TOKEN"] = json!(SECRET);
    let response = server.run(payload).await;
    assert_eq!(response.status, StatusCode::OK);
    let (command_pid, child_pid) = script.pids().await;

    kill(server.pid(), Signal::SIGTERM).unwrap();
    let output = server.wait_with_output().await;

    assert!(output.status.success());
    assert!(script.was_terminated());
    wait_until_gone(command_pid).await;
    wait_until_gone(child_pid).await;
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!logs.contains(SECRET));
}
