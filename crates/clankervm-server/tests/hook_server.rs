mod support;

use clankervm_server::HookServerError;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::fs;
use std::time::Duration;
use support::{
    IGNORE_TERM_TRAP, RunScriptBuilder, TestServerBuilder, executable_script, run_request,
    wait_until_gone,
};
use tempfile::tempdir;

const SECRET: &str = "github-token-that-must-not-leak";

#[tokio::test]
async fn ready_is_available_and_terminate_stops_an_idle_server() {
    let server = TestServerBuilder::new().start().await;

    let ready = server.post("/ready").await;
    assert_eq!(ready.status, StatusCode::OK);
    assert_eq!(ready.body, json!({ "status": "ready" }));

    let terminate = server.post("/terminate").await;
    assert_eq!(terminate.status, StatusCode::OK);
    assert_eq!(terminate.body, json!({ "status": "terminating" }));
    server.wait().await.unwrap();
}

#[tokio::test]
async fn run_passes_arguments_environment_and_microvm_id_to_the_command() {
    let directory = tempdir().unwrap();
    let executable = executable_script(
        directory.path(),
        "capture-command",
        "printf '%s\n%s\n%s\n%s' \"$1\" \"$2\" \"$AWS_LAMBDA_MICROVM_ID\" \"$RUN_SECRET\" > \"$OUTPUT\"",
    );
    let output = directory.path().join("output");
    let server = TestServerBuilder::new().start().await;

    let response = server
        .run(json!({
            "command": executable,
            "args": ["first", "$(not shell syntax)"],
            "environment": {
                "OUTPUT": output,
                "RUN_SECRET": "secret-value"
            }
        }))
        .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, json!({ "status": "accepted" }));
    server.wait().await.unwrap();
    assert_eq!(
        fs::read_to_string(output).unwrap(),
        "first\n$(not shell syntax)\nmicrovm-123\nsecret-value"
    );
}

#[tokio::test]
async fn invalid_requests_do_not_claim_the_run_and_a_second_run_conflicts() {
    let server = TestServerBuilder::new().start().await;
    let invalid = [
        json!({ "microvmId": "id", "runHookPayload": "{" }),
        json!({ "microvmId": "id", "runHookPayload": "{}" }),
        json!({ "microvmId": "", "runHookPayload": "{\"command\":\"run\"}" }),
        json!({ "microvmId": "id", "runHookPayload": "{\"command\":\"\"}" }),
        json!({ "microvmId": "id", "runHookPayload": "{\"command\":\"run\",\"unknown\":true}" }),
        run_request(&json!({ "command": "run", "environment": { "BAD=KEY": "value" } })),
        run_request(&json!({ "command": "run", "environment": { "KEY": "bad\u{0}value" } })),
        run_request(&json!({ "command": "run", "args": ["bad\u{0}argument"] })),
    ];
    for body in invalid {
        let response = server.post_json("/run", body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(response.body.get("error").is_some());
    }

    assert_eq!(
        server.run(long_running_command()).await.status,
        StatusCode::OK
    );
    let conflict = server.run(long_running_command()).await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.body, json!({ "error": "run already started" }));
    server.post("/terminate").await;
    server.wait().await.unwrap();
}

#[tokio::test]
async fn unknown_or_unsupported_routes_return_json_errors() {
    let server = TestServerBuilder::new().start().await;

    for response in [server.get("/ready").await, server.post("/missing").await] {
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.body, json!({ "error": "not found" }));
    }
    server.post("/terminate").await;
    server.wait().await.unwrap();
}

#[tokio::test]
async fn spawn_failure_is_returned_by_the_run_hook() {
    let server = TestServerBuilder::new().start().await;

    let response = server
        .run(json!({ "command": "/path/that/does/not/exist" }))
        .await;

    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response.body.get("error").is_some());
    assert!(matches!(
        server.wait().await,
        Err(HookServerError::CommandSpawn(_))
    ));
}

#[tokio::test]
async fn invalid_payloads_do_not_expose_environment_secrets() {
    let server = TestServerBuilder::new().start().await;
    let payloads = [
        json!({
            "command": "/bin/true",
            "environment": { "GITHUB_TOKEN": format!("{SECRET}\u{0}") }
        }),
        json!({ "command": "/bin/true", "environment": SECRET }),
    ];

    for payload in payloads {
        let response = server.run(payload).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(!response.body.to_string().contains(SECRET));
    }
}

#[tokio::test]
async fn spawn_failure_does_not_expose_environment_secrets() {
    let server = TestServerBuilder::new().start().await;

    let response = server
        .run(json!({
            "command": "/path/that/does/not/exist",
            "environment": { "GITHUB_TOKEN": SECRET }
        }))
        .await;

    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!response.body.to_string().contains(SECRET));
    let error = server.wait().await.unwrap_err();
    assert!(!error.to_string().contains(SECRET));
}

#[tokio::test]
async fn failed_command_is_returned_after_the_accepted_response() {
    let server = TestServerBuilder::new().start().await;

    let response = server
        .run(json!({ "command": "/bin/sh", "args": ["-c", "exit 7"] }))
        .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, json!({ "status": "accepted" }));
    assert!(matches!(
        server.wait().await,
        Err(HookServerError::CommandFailed)
    ));
}

#[tokio::test]
async fn terminate_allows_the_command_process_group_to_exit_gracefully() {
    let script = RunScriptBuilder::new("command-with-child").build();
    let server = TestServerBuilder::new()
        .terminate_grace_period(Duration::from_secs(2))
        .start()
        .await;

    let response = server.run(script.payload()).await;

    assert_eq!(response.status, StatusCode::OK);
    let (command_pid, child_pid) = script.pids().await;
    let (first_terminate, second_terminate) =
        tokio::join!(server.post("/terminate"), server.post("/terminate"));

    for terminate in [first_terminate, second_terminate] {
        assert_eq!(terminate.status, StatusCode::OK);
        assert_eq!(terminate.body, json!({ "status": "terminating" }));
    }

    assert!(script.was_terminated());
    wait_until_gone(command_pid).await;
    wait_until_gone(child_pid).await;
    server.wait().await.unwrap();
}

#[tokio::test]
async fn terminate_waits_for_descendants_after_the_command_exits() {
    let script = RunScriptBuilder::new("command-that-leaves-child")
        .child("(trap '' TERM; exec sleep 60)")
        .build();
    let server = TestServerBuilder::new()
        .terminate_grace_period(Duration::from_millis(50))
        .start()
        .await;

    let response = server.run(script.payload()).await;

    assert_eq!(response.status, StatusCode::OK);
    let (command_pid, child_pid) = script.pids().await;
    let terminate = server.post("/terminate").await;

    assert_eq!(terminate.status, StatusCode::OK);
    assert!(script.was_terminated());
    wait_until_gone(command_pid).await;
    wait_until_gone(child_pid).await;
    server.wait().await.unwrap();
}

#[tokio::test]
async fn terminate_kills_the_process_group_after_the_grace_period() {
    let script = RunScriptBuilder::new("command-that-ignores-term")
        .trap(IGNORE_TERM_TRAP)
        .build();
    let server = TestServerBuilder::new()
        .terminate_grace_period(Duration::from_millis(50))
        .start()
        .await;

    let response = server.run(script.payload()).await;
    assert_eq!(response.status, StatusCode::OK);
    let (command_pid, child_pid) = script.pids().await;

    let terminate = server.post("/terminate").await;

    assert_eq!(terminate.status, StatusCode::OK);
    assert_eq!(terminate.body, json!({ "status": "terminating" }));
    wait_until_gone(command_pid).await;
    wait_until_gone(child_pid).await;
    server.wait().await.unwrap();
}

fn long_running_command() -> Value {
    json!({ "command": "/bin/sh", "args": ["-c", "exec sleep 60"] })
}
