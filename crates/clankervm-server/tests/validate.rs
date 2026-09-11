mod support;

use clankervm_server::HookServerError;
use nix::sys::signal::{Signal, kill};
use reqwest::StatusCode;
use serde_json::json;
use std::fs;
use std::time::Duration;
use support::{IGNORE_TERM_TRAP, RunScriptBuilder, TestServer, TestServerBuilder, wait_until_gone};

#[tokio::test]
async fn validation_is_lazy_once_only_and_independent_of_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let release = directory.path().join("release");
    let server = TestServerBuilder::new().validate_command(json!({
        "command": "/bin/sh",
        "args": ["-c", "printf '%s\\n' \"$1\" >> \"$OUTPUT\"; while [ ! -f \"$RELEASE\" ]; do sleep 0.01; done", "validate", "$(literal argument)"],
        "environment": {"OUTPUT": output, "RELEASE": release}
    })).start().await;
    server.wait_until_ready().await;
    assert!(!output.exists());
    tokio::time::timeout(Duration::from_secs(2), async {
        let (first, second) = tokio::join!(server.post("/validate"), server.post("/validate"));
        for response in [first, second] {
            assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.body, json!({"status": "validating"}));
        }
        assert_eq!(
            server.run(json!({"command": "/usr/bin/true"})).await.status,
            StatusCode::CONFLICT
        );
    })
    .await
    .expect("validation blocked hook requests");
    fs::write(release, "continue").unwrap();
    wait_until_validated(&server).await;
    assert_eq!(
        server.post("/validate").await.body,
        json!({"status": "validated"})
    );
    assert_eq!(fs::read_to_string(output).unwrap(), "$(literal argument)\n");
    assert_eq!(
        server.run(json!({"command": "/usr/bin/true"})).await.status,
        StatusCode::OK
    );
    server.wait().await.unwrap();
}

#[tokio::test]
async fn validation_is_not_required_on_a_normal_run_and_cannot_start_during_it() {
    let server = TestServerBuilder::new()
        .validate_command(json!({"command": "/path/that/does/not/exist"}))
        .start()
        .await;
    assert_eq!(
        server
            .run(json!({"command": "/bin/sh", "args": ["-c", "exec sleep 60"]}))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(server.post("/validate").await.status, StatusCode::CONFLICT);
    server.post("/terminate").await;
    server.wait().await.unwrap();
}

#[tokio::test]
async fn validation_waits_for_initialization_and_is_absent_without_configuration() {
    let idle = TestServerBuilder::new().start().await;
    assert_eq!(idle.post("/validate").await.status, StatusCode::NOT_FOUND);
    idle.post("/terminate").await;
    idle.wait().await.unwrap();

    let script = RunScriptBuilder::new("initializing").build();
    let server = TestServerBuilder::new()
        .ready_command(script.payload())
        .validate_command(json!({"command": "/path/that/does/not/exist"}))
        .start()
        .await;
    let (parent, child) = script.pids().await;
    assert_eq!(
        server.post("/validate").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    server.post("/terminate").await;
    server.wait().await.unwrap();
    wait_until_gone(parent).await;
    wait_until_gone(child).await;
}

#[tokio::test]
async fn validation_failures_stop_the_server_without_passing_validation() {
    for (command, spawn_failure) in [
        (json!({"command": "/path/that/does/not/exist"}), true),
        (
            json!({"command": "/bin/sh", "args": ["-c", "exit 7"]}),
            false,
        ),
    ] {
        let server = TestServerBuilder::new()
            .validate_command(command)
            .start()
            .await;
        server.wait_until_ready().await;
        let response = server.post("/validate").await;
        let error = server.wait().await.unwrap_err();
        if spawn_failure {
            assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(matches!(error, HookServerError::CommandSpawn(_)));
        } else {
            assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(matches!(error, HookServerError::CommandFailed));
        }
    }
}

#[tokio::test]
async fn terminate_cancels_validation_and_its_descendants() {
    let script = RunScriptBuilder::new("validate-with-child")
        .trap(IGNORE_TERM_TRAP)
        .build();
    let server = TestServerBuilder::new()
        .validate_command(script.payload())
        .start()
        .await;
    assert_eq!(
        server.post("/validate").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let (parent, child) = script.pids().await;
    let (first, second) = tokio::join!(server.post("/terminate"), server.post("/terminate"));
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);
    server.wait().await.unwrap();
    wait_until_gone(parent).await;
    wait_until_gone(child).await;
}

#[tokio::test]
async fn image_environment_configures_validation_and_sigterm_cancels_it() {
    let script = RunScriptBuilder::new("validate-signal").build();
    let server = TestServerBuilder::new()
        .terminate_grace_period(Duration::from_secs(1))
        .validate_command(script.payload())
        .start_process();
    server.wait_until_ready().await;
    assert_eq!(
        server.post("/validate").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let (parent, child) = script.pids().await;
    kill(server.pid(), Signal::SIGTERM).unwrap();
    assert!(server.wait_with_output().await.status.success());
    assert!(script.was_terminated());
    wait_until_gone(parent).await;
    wait_until_gone(child).await;
}

#[tokio::test]
async fn invalid_validate_payload_does_not_expose_environment_values() {
    let secret = "validate-secret-that-must-not-leak";
    let server = TestServerBuilder::new()
        .terminate_grace_period(Duration::from_secs(1))
        .validate_command(json!({"command": "/usr/bin/true", "environment": secret}))
        .start_process();
    let output = server.wait_with_output().await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid validate hook payload"));
    assert!(!stderr.contains(secret));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
}

async fn wait_until_validated(server: &TestServer) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = server.post("/validate").await;
            if response.status == StatusCode::OK {
                return;
            }
            assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("validation did not finish");
}
