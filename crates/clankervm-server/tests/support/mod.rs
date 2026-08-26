#![allow(dead_code)]

use clankervm_server::{BASE_PATH, HookServerError, LambdaHookServer};
use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::fs;
use std::net::TcpListener as StandardTcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

pub const GRACEFUL_TERM_TRAP: &str = r#"trap 'printf terminated > "$TERMINATED"; exit 0' TERM"#;
pub const IGNORE_TERM_TRAP: &str = "trap '' TERM";

pub struct TestServerBuilder {
    terminate_grace_period: Duration,
}

impl TestServerBuilder {
    pub fn new() -> Self {
        Self {
            terminate_grace_period: Duration::from_millis(100),
        }
    }

    pub fn terminate_grace_period(mut self, terminate_grace_period: Duration) -> Self {
        self.terminate_grace_period = terminate_grace_period;
        self
    }

    pub async fn start(self) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let join = tokio::spawn(
            LambdaHookServer::with_terminate_grace_period(self.terminate_grace_period)
                .serve(listener),
        );
        TestServer {
            base_url: format!("http://{address}{BASE_PATH}"),
            client: Client::new(),
            join,
        }
    }

    pub fn start_process(self) -> TestServerProcess {
        let grace_period_seconds = self.terminate_grace_period.as_secs();
        assert_eq!(
            self.terminate_grace_period,
            Duration::from_secs(grace_period_seconds),
            "process-backed test servers require a whole-second terminate grace period"
        );

        let port = available_port();
        let child = Command::new(env!("CARGO_BIN_EXE_clankervm-server"))
            .args([
                "--port",
                &format!("127.0.0.1:{port}"),
                "--terminate-grace-period",
                &grace_period_seconds.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        TestServerProcess {
            base_url: format!("http://127.0.0.1:{port}{BASE_PATH}"),
            client: Client::new(),
            child,
        }
    }
}

pub struct TestServer {
    base_url: String,
    client: Client,
    join: JoinHandle<Result<(), HookServerError>>,
}

pub struct TestResponse {
    pub status: StatusCode,
    pub body: Value,
}

impl TestServer {
    pub async fn post(&self, path: &str) -> TestResponse {
        request(&self.client, &self.base_url, Method::POST, path, None).await
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        request(&self.client, &self.base_url, Method::GET, path, None).await
    }

    pub async fn run(&self, payload: Value) -> TestResponse {
        self.post_json("/run", run_request(&payload)).await
    }

    pub async fn post_json(&self, path: &str, body: Value) -> TestResponse {
        request(&self.client, &self.base_url, Method::POST, path, Some(body)).await
    }

    pub async fn wait(self) -> Result<(), HookServerError> {
        self.join.await.unwrap()
    }
}

pub struct TestServerProcess {
    base_url: String,
    client: Client,
    child: Child,
}

impl TestServerProcess {
    pub async fn wait_until_ready(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .client
                    .post(format!("{}/ready", self.base_url))
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == StatusCode::OK)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server did not become ready");
    }

    pub async fn run(&self, payload: Value) -> TestResponse {
        request(
            &self.client,
            &self.base_url,
            Method::POST,
            "/run",
            Some(run_request(&payload)),
        )
        .await
    }

    pub fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id().unwrap().cast_signed())
    }

    pub async fn wait_with_output(self) -> Output {
        tokio::time::timeout(Duration::from_secs(5), self.child.wait_with_output())
            .await
            .expect("server did not stop")
            .unwrap()
    }
}

async fn request(
    client: &Client,
    base_url: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> TestResponse {
    let request = client.request(method, format!("{base_url}{path}"));
    let response = match body {
        Some(body) => request.json(&body).send().await.unwrap(),
        None => request.send().await.unwrap(),
    };
    let status = response.status();
    let body = response.json().await.unwrap();
    TestResponse { status, body }
}

pub struct RunScriptBuilder {
    name: String,
    trap: String,
    child: String,
}

impl RunScriptBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            trap: GRACEFUL_TERM_TRAP.to_string(),
            child: "sleep 60".to_string(),
        }
    }

    pub fn trap(mut self, trap: &str) -> Self {
        self.trap = trap.to_string();
        self
    }

    pub fn child(mut self, child: &str) -> Self {
        self.child = child.to_string();
        self
    }

    pub fn build(self) -> RunScript {
        let directory = tempfile::tempdir().unwrap();
        let pids = directory.path().join("pids");
        let terminated = directory.path().join("terminated");
        let executable = executable_script(
            directory.path(),
            &self.name,
            &format!(
                "{trap}\n{child} &\necho \"$$ $!\" > \"$PIDS\"\nwait",
                trap = self.trap,
                child = self.child
            ),
        );
        RunScript {
            _directory: directory,
            executable,
            pids,
            terminated,
        }
    }
}

/// A run command that spawns a background child, records both process IDs,
/// and waits. Its trap decides how it responds to SIGTERM.
pub struct RunScript {
    _directory: TempDir,
    executable: PathBuf,
    pids: PathBuf,
    terminated: PathBuf,
}

impl RunScript {
    pub fn payload(&self) -> Value {
        json!({
            "command": self.executable,
            "environment": { "PIDS": self.pids, "TERMINATED": self.terminated }
        })
    }

    pub async fn pids(&self) -> (Pid, Pid) {
        read_pids(&self.pids).await
    }

    pub fn was_terminated(&self) -> bool {
        fs::read_to_string(&self.terminated).is_ok_and(|contents| contents == "terminated")
    }
}

pub fn run_request(payload: &Value) -> Value {
    json!({
        "microvmId": "microvm-123",
        "runHookPayload": serde_json::to_string(payload).unwrap()
    })
}

pub fn executable_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let executable = directory.join(name);
    fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

async fn read_pids(path: &Path) -> (Pid, Pid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                let pids = contents
                    .split_whitespace()
                    .map(|pid| Pid::from_raw(pid.parse::<i32>().unwrap()))
                    .collect::<Vec<_>>();
                if let [command, child] = pids.as_slice() {
                    return (*command, *child);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run command did not write its process IDs")
}

pub async fn wait_until_gone(pid: Pid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match kill(pid, None) {
                Err(Errno::ESRCH) => return,
                Err(error) => panic!("failed to check run process: {error}"),
                Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("run process remained after server shutdown");
}

fn available_port() -> u16 {
    StandardTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
