use clankervm_server::{BASE_PATH, HookServerError, LambdaHookServer};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::tempdir;
use tokio::net::TcpListener;

#[tokio::test]
async fn ready_is_available_and_terminate_stops_an_idle_server() {
    let server = TestServer::start().await;

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
    let server = TestServer::start().await;

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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;

    for response in [server.get("/ready").await, server.post("/missing").await] {
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.body, json!({ "error": "not found" }));
    }
    server.post("/terminate").await;
    server.wait().await.unwrap();
}

#[tokio::test]
async fn spawn_failure_is_returned_by_the_run_hook() {
    let server = TestServer::start().await;

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
async fn failed_command_is_returned_after_the_accepted_response() {
    let server = TestServer::start().await;

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
async fn terminate_waits_until_the_command_process_group_is_gone() {
    let directory = tempdir().unwrap();
    let pids = directory.path().join("pids");
    let executable = executable_script(
        directory.path(),
        "command-with-child",
        "sleep 60 &\necho \"$$ $!\" > \"$PIDS\"\nwait",
    );
    let server = TestServer::start().await;

    let response = server
        .run(json!({ "command": executable, "environment": { "PIDS": pids } }))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let (command_pid, child_pid) = read_pids(&pids).await;

    let terminate = server.post("/terminate").await;

    assert_eq!(terminate.status, StatusCode::OK);
    assert_eq!(terminate.body, json!({ "status": "terminating" }));
    wait_until_gone(command_pid).await;
    wait_until_gone(child_pid).await;
    server.wait().await.unwrap();
}

fn run_request(payload: &Value) -> Value {
    json!({
        "microvmId": "microvm-123",
        "runHookPayload": serde_json::to_string(payload).unwrap()
    })
}

fn long_running_command() -> Value {
    json!({ "command": "/bin/sh", "args": ["-c", "exec sleep 60"] })
}

fn executable_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let executable = directory.join(name);
    fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

async fn wait_until_gone(pid: Pid) {
    while kill(pid, None).is_ok() {
        tokio::task::yield_now().await;
    }
}

async fn read_pids(path: &Path) -> (Pid, Pid) {
    while !path.exists() {
        tokio::task::yield_now().await;
    }
    let contents = fs::read_to_string(path).unwrap();
    let mut pids = contents
        .split_whitespace()
        .map(|pid| pid.parse::<i32>().unwrap())
        .map(Pid::from_raw);
    (pids.next().unwrap(), pids.next().unwrap())
}

struct TestServer {
    base_url: String,
    client: Client,
    join: tokio::task::JoinHandle<Result<(), HookServerError>>,
}

struct TestResponse {
    status: StatusCode,
    body: Value,
}

impl TestServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let join = tokio::spawn(LambdaHookServer::new().serve(listener));
        Self {
            base_url: format!("http://{address}{BASE_PATH}"),
            client: Client::new(),
            join,
        }
    }

    async fn post(&self, path: &str) -> TestResponse {
        self.request(Method::POST, path, None).await
    }

    async fn get(&self, path: &str) -> TestResponse {
        self.request(Method::GET, path, None).await
    }

    async fn run(&self, payload: Value) -> TestResponse {
        self.request(Method::POST, "/run", Some(run_request(&payload)))
            .await
    }

    async fn post_json(&self, path: &str, body: Value) -> TestResponse {
        self.request(Method::POST, path, Some(body)).await
    }

    async fn wait(self) -> Result<(), HookServerError> {
        self.join.await.unwrap()
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> TestResponse {
        let request = self
            .client
            .request(method, format!("{}{path}", self.base_url));
        let response = match body {
            Some(body) => request.json(&body).send().await.unwrap(),
            None => request.send().await.unwrap(),
        };
        let status = response.status();
        let body = response.json().await.unwrap();
        TestResponse { status, body }
    }
}
