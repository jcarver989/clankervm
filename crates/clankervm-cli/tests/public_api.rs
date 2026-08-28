use clankervm::{Cli, Command as ClankerCommand, PayloadError, build_run_payload};
use clap::Parser;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use tempfile::TempDir;

#[test]
fn help_exposes_release_workflow() {
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in ["init", "push", "status", "run"] {
        assert!(text.contains(command), "missing {command} in {text}");
    }
    assert!(
        !text.contains("  bundle"),
        "unexpected bundle command in {text}"
    );
    assert!(
        !text.contains("  wait"),
        "unexpected wait command in {text}"
    );
}

#[test]
fn run_requires_separator_before_the_command() {
    let cli = Cli::try_parse_from([
        "clankervm",
        "run",
        "--execution-role-arn",
        "arn:aws:iam::123456789012:role/demo",
        "--",
        "command",
        "--command-option",
    ])
    .unwrap();
    let ClankerCommand::Run(run) = cli.command else {
        panic!("expected run command");
    };
    assert_eq!(run.command, ["command", "--command-option"]);
}

#[test]
fn push_flags_mirror_flat_config_values() {
    let cli = Cli::try_parse_from([
        "clankervm",
        "push",
        "--context",
        "image",
        "--artifact-bucket",
        "artifacts",
        "--build-role-arn",
        "arn:aws:iam::123456789012:role/build",
        "--capability",
        "ALL",
        "--tag",
        "team=platform",
        "--tag",
        "environment=test",
        "--ready-timeout-seconds",
        "120",
    ])
    .unwrap();
    let ClankerCommand::Push(push) = cli.command else {
        panic!("expected push command");
    };

    assert_eq!(
        push.config.context.as_deref(),
        Some(std::path::Path::new("image"))
    );
    assert_eq!(push.config.artifact_bucket.as_deref(), Some("artifacts"));
    assert_eq!(push.config.capabilities.unwrap()[0].as_str(), "ALL");
    assert_eq!(
        push.config.tags.unwrap(),
        ["team=platform", "environment=test"]
    );
    assert_eq!(push.config.ready_timeout_seconds, Some(120));
}

#[test]
fn push_accepts_a_directory_or_zip_path() {
    for path in ["prepared-directory", "prepared-image.zip"] {
        let cli = Cli::try_parse_from(["clankervm", "push", path]).unwrap();
        let ClankerCommand::Push(push) = cli.command else {
            panic!("expected push command");
        };
        assert_eq!(push.source.as_deref(), Some(std::path::Path::new(path)));
    }

    assert!(Cli::try_parse_from(["clankervm", "push", "image", "--bundle", "image.zip"]).is_err());
}

#[test]
fn status_owns_waiting_and_removed_options_are_rejected() {
    let cli = Cli::try_parse_from(["clankervm", "status", "demo@2", "--wait", "--timeout", "5m"])
        .unwrap();
    let ClankerCommand::Status(status) = cli.command else {
        panic!("expected status command");
    };
    assert!(status.wait);
    assert_eq!(status.release.as_deref(), Some("demo@2"));

    for args in [
        vec!["clankervm", "bundle"],
        vec!["clankervm", "wait"],
        vec!["clankervm", "push", "--detach"],
        vec!["clankervm", "push", "--image", "agent"],
        vec!["clankervm", "status", "--image", "agent"],
        vec!["clankervm", "run", "--image", "agent", "--", "echo"],
        vec!["clankervm", "push", "--poll-interval", "1s"],
        vec!["clankervm", "push", "--capability", "NOPE"],
        vec!["clankervm", "push", "--timeout", "eventually"],
        vec!["clankervm", "run", "--env", "X=1", "--", "echo"],
        vec!["clankervm", "run", "--script", "run.sh", "--", "echo"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn init_only_creates_the_project_file() {
    let directory = TempDir::new().unwrap();
    let config = directory.path().join("clankervm.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "init",
            "--name",
            "demo",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(config.is_file());
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("schema-version = 1"), "{text}");
    assert!(text.contains("name = \"demo\""), "{text}");
    assert!(text.contains("[push]"), "{text}");
    assert!(text.contains("# artifact-bucket = "), "{text}");
    assert!(text.contains("# build-role-arn = "), "{text}");
    assert!(text.contains("[run]"), "{text}");
    assert!(text.contains("# execution-role-arn = "), "{text}");
    assert!(!text.contains("[bundle]"), "{text}");
    assert!(text.contains("[image]"), "{text}");
    assert!(!text.contains("[app]"), "{text}");
    assert!(!directory.path().join(".gitignore").exists());
    assert!(!directory.path().join(".clankervm").exists());
}

#[test]
fn init_emits_json_when_requested() {
    let directory = TempDir::new().unwrap();
    let config = directory.path().join("clankervm.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
            "init",
            "--name",
            "demo",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["configPath"], config.to_str().unwrap());
}

#[test]
fn status_resolves_the_image_from_the_execution_role_alone() {
    let directory = TempDir::new().unwrap();
    fs::write(
        directory.path().join("clankervm.toml"),
        r#"schema-version = 1
[image]
name = "demo"
region = "us-east-1"
[run]
execution-role-arn = "arn:aws:iam::123456789012:role/run"
"#,
    )
    .unwrap();
    let image_created = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATED","latestActiveImageVersion":"2","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
    let version_active = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
    let fake = FakeAws::start(vec![
        Response {
            status: 200,
            body: image_created,
        },
        Response {
            status: 200,
            body: version_active,
        },
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .current_dir(directory.path())
        .args(["--format", "json", "status"])
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_ENDPOINT_URL_LAMBDA_MICROVMS", fake.url())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["release"], "demo@2");
    assert_eq!(
        result["imageArn"],
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
    );
    fake.finish();
}

#[test]
fn raw_run_payload_preserves_command_and_args() {
    let payload = build_run_payload("echo", &["hello world".into()], "us-east-1").unwrap();
    let json: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(json["command"], "echo");
    assert_eq!(json["args"], serde_json::json!(["hello world"]));
}

#[test]
fn oversized_command_payload_is_rejected() {
    let error = build_run_payload("sh", &["x".repeat(5000)], "region").unwrap_err();
    assert!(matches!(error, PayloadError::TooLarge { .. }));
}

#[test]
fn push_waits_for_the_exact_version_to_become_active() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path());
    fs::write(directory.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let image_creating = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATING","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
    let image_created = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATED","latestActiveImageVersion":"2","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
    let version_pending = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"PENDING","status":"INACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
    let version_active = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
    let fake = FakeAws::start(vec![
        Response {
            status: 200,
            body: "{}",
        },
        Response {
            status: 404,
            body: r#"{"__type":"ResourceNotFoundException"}"#,
        },
        Response {
            status: 200,
            body: image_creating,
        },
        Response {
            status: 200,
            body: image_created,
        },
        Response {
            status: 200,
            body: version_pending,
        },
        Response {
            status: 200,
            body: image_created,
        },
        Response {
            status: 200,
            body: version_active,
        },
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .current_dir(directory.path())
        .args(["--format", "json", "push", "--timeout", "5s"])
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_ENDPOINT_URL", fake.url())
        .env("AWS_ENDPOINT_URL_LAMBDA_MICROVMS", fake.url())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["release"], "demo@2");
    assert_eq!(result["versionState"], "SUCCESSFUL");
    assert_eq!(result["versionStatus"], "ACTIVE");
    let requests = fake.finish();
    assert_eq!(requests.len(), 7);
    assert!(requests[0].contains("Dockerfile"));
}

#[test]
fn run_uses_project_defaults_and_forwards_client_token() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path());
    let fake = FakeAws::start(vec![Response {
        status: 200,
        body: r#"{"microvmId":"microvm-123","state":"PENDING","endpoint":"https://example.test","imageArn":"image","imageVersion":"7","maximumDurationInSeconds":3600,"startedAt":1787616000}"#,
    }]);
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .current_dir(directory.path())
        .args(["--format", "json", "run", "--client-token", "run-42"])
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_ENDPOINT_URL_LAMBDA_MICROVMS", fake.url())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["microvmId"], "microvm-123");
    assert_eq!(result["imageVersion"], "7");
    let request = fake.finish().pop().unwrap();
    assert!(request.contains("run-42"));
    assert!(request.contains("echo"));
    assert!(request.contains("hello"));
}

fn write_config(directory: &std::path::Path) {
    fs::write(
        directory.join("clankervm.toml"),
        r#"schema-version = 1
[image]
name = "demo"
region = "us-east-1"
[push]
artifact-bucket = "bucket"
build-role-arn = "arn:aws:iam::123456789012:role/build"
[run]
command = ["echo", "hello"]
execution-role-arn = "arn:aws:iam::123456789012:role/run"
log-group = "/demo/runs"
"#,
    )
    .unwrap();
}

struct FakeAws {
    address: String,
    join: thread::JoinHandle<Vec<String>>,
}

struct Response {
    status: u16,
    body: &'static str,
}

impl FakeAws {
    fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let join = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..n]);
                    if n == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers_end = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < headers_end + length {
                    let read = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                }
                requests.push(String::from_utf8_lossy(&bytes).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                )
                .unwrap();
            }
            requests
        });
        Self { address, join }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn finish(self) -> Vec<String> {
        self.join.join().unwrap()
    }
}
