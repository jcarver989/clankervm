mod support;

use serde_json::Value;
use std::fs;
use std::process::{Command, Output};
use support::{FakeAws, Response};
use tempfile::TempDir;

const RUNNING: &str = r#"{"microvmId":"vm-1","state":"RUNNING","endpoint":"https://test.lambda-microvm.us-east-1.on.aws","imageArn":"image","imageVersion":"1","executionRoleArn":"role","startedAt":1787616000,"maximumDurationInSeconds":3600,"ingressNetworkConnectors":["arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS"]}"#;
const TERMINATING: &str = r#"{"microvmId":"vm-1","state":"TERMINATING","endpoint":"","imageArn":"image","imageVersion":"1","executionRoleArn":"role","startedAt":1787616000,"maximumDurationInSeconds":3600}"#;

#[test]
fn application_token_sdk_request_is_single_port_and_response_never_printed() {
    let project = Project::new().config("", "");
    let aws = FakeAws::start(vec![
        Response::ok(RUNNING),
        Response::ok(r#"{"authToken":{"X-aws-proxy-auth":"SENTINEL SECRET MUST NOT LEAK"}}"#),
    ]);
    let output = project.run(&["connect", "vm-1", "web"], &aws.url());
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SENTINEL"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SENTINEL"));
    let requests = aws.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("/microvms/vm-1/auth-token"));
    let body: Value = serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["expirationInMinutes"], 60);
    assert_eq!(body["allowedPorts"], serde_json::json!([{ "port": 3000 }]));
    assert!(
        !requests
            .iter()
            .any(|request| request.contains("shell-auth-token") || request.contains("terminate"))
    );
}

#[test]
fn client_retries_use_fresh_sdk_tokens_without_probing_or_printing_them() {
    let project = Project::new().config("", "client=['sh', './client.sh', '{auth-header}']");
    fs::write(project.directory.path().join("client.sh"), "[ \"$1\" = 'X-aws-proxy-auth: SENTINEL-first' ] && exit 37\n[ \"$1\" = 'X-aws-proxy-auth: SENTINEL-second' ]").unwrap();
    let aws = FakeAws::start(vec![
        Response::ok(RUNNING),
        Response::ok(r#"{"authToken":{"X-aws-proxy-auth":"SENTINEL-first"}}"#),
        Response::ok(r#"{"authToken":{"X-aws-proxy-auth":"SENTINEL-second"}}"#),
    ]);
    let output = project.run(
        &["connect", "vm-1", "web", "--connect-timeout", "5s"],
        &aws.url(),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SENTINEL"));
    let requests = aws.finish();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].split("\r\n\r\n").nth(1),
        requests[2].split("\r\n\r\n").nth(1)
    );
}

#[test]
fn describe_and_stop_are_credential_free_and_idempotent() {
    let project = Project::new().config("", "");
    let aws = FakeAws::start(vec![Response::ok(RUNNING)]);
    let output = project.run(&["--format", "json", "describe", "vm-1"], &aws.url());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["microvmId"], "vm-1");
    assert_eq!(body["state"], "RUNNING");
    assert!(body["stateReason"].is_null());
    assert_eq!(aws.finish().len(), 1);
    for (response, confirmed) in [
        (Response::not_found(), true),
        (Response::ok(TERMINATING), false),
    ] {
        let aws = FakeAws::start(vec![response]);
        let output = project.run(&["--format", "json", "stop", "vm-1"], &aws.url());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["terminationConfirmed"], confirmed);
        assert_eq!(aws.finish().len(), 1);
    }
}

struct Project {
    directory: TempDir,
}
impl Project {
    fn new() -> Self {
        Self {
            directory: TempDir::new().unwrap(),
        }
    }
    fn config(self, aws: &str, connection: &str) -> Self {
        let connection = if connection.is_empty() {
            "client=['true']"
        } else {
            connection
        };
        fs::write(self.directory.path().join("clankervm.toml"), format!("[aws]\nregion='us-east-1'\n{aws}\n[microvm]\nname='test'\n[microvm.run]\ncommand=['/remote-only']\niam-role='arn:aws:iam::123456789012:role/run'\n[microvm.run.network]\ningress='ALL_INGRESS'\n[microvm.run.connect.web]\nport=3000\nprotocol='http'\n{connection}\n")).unwrap();
        self
    }
    fn command(&self, args: &[&str], endpoint: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_clankervm"));
        command
            .current_dir(self.directory.path())
            .args(args)
            .env("AWS_ENDPOINT_URL", endpoint)
            .env("AWS_ACCESS_KEY_ID", "test")
            .env("AWS_SECRET_ACCESS_KEY", "test")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_MAX_ATTEMPTS", "1")
            .env("AWS_CONFIG_FILE", "/dev/null")
            .env("AWS_SHARED_CREDENTIALS_FILE", "/dev/null");
        command
    }
    fn run(&self, args: &[&str], endpoint: &str) -> Output {
        self.command(args, endpoint).output().unwrap()
    }
}
