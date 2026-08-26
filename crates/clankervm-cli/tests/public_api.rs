use clankervm::{
    Cli, Command as ClankerCommand, PayloadError, RuntimeEnvironment, build_run_payload,
    parse_env_assignments, parse_env_file, zip_context,
};
use clap::Parser;
use serde_json::Value;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use tempfile::TempDir;

#[test]
fn help_exposes_docker_shaped_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("build") && text.contains("run"));
}

#[test]
fn run_accepts_command_options_after_the_image() {
    let cli = Cli::try_parse_from([
        "clankervm",
        "run",
        "--region",
        "us-east-1",
        "--execution-role-arn",
        "arn:role",
        "demo",
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
fn raw_run_payload_preserves_command_and_args() {
    let payload = build_run_payload(
        "echo",
        &["hello world".into()],
        &RuntimeEnvironment::default(),
        "us-east-1",
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(json["command"], "echo");
    assert_eq!(json["args"], serde_json::json!(["hello world"]));
}

#[test]
fn script_and_literal_env_payload_are_publicly_constructible() {
    let env = parse_env_file("TOKEN=abc=def\n").unwrap();
    let payload = build_run_payload(
        "/bin/sh",
        &["-c".into(), "echo $TOKEN".into(), "script.sh".into()],
        &env,
        "x",
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(json["environment"]["TOKEN"], "abc=def");
    assert_eq!(json["args"][0], "-c");
}

#[test]
fn oversized_payload_names_keys_without_secrets() {
    let env = parse_env_file(&format!("SECRET={}\n", "x".repeat(5000))).unwrap();
    let error = build_run_payload("sh", &[], &env, "region").unwrap_err();
    assert!(matches!(error, PayloadError::TooLarge { .. }));
    assert!(error.to_string().contains("SECRET"));
    assert!(!error.to_string().contains(&"x".repeat(100)));
}

#[test]
fn env_assignment_without_equals_inherits_host() {
    let host_path = std::env::var("PATH").unwrap();
    let env = parse_env_assignments(&["PATH".into()]).unwrap();
    assert_eq!(env["PATH"], host_path);
}

#[test]
fn build_propagates_fake_aws_errors_instead_of_creating() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("source.txt"), "content").unwrap();
    let fake = FakeAws::start_status(403);
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .args([
            "build",
            "-t",
            "demo",
            "--artifact-bucket",
            "bucket",
            "--build-role-arn",
            "arn:role",
            dir.path().to_str().unwrap(),
        ])
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCOUNT_ID", "123456789012")
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_ENDPOINT_URL", fake.url())
        .env("AWS_ENDPOINT_URL_LAMBDA_MICROVMS", fake.url())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to get image"));
    assert_eq!(fake.finish().len(), 2);
}

#[test]
fn generic_context_is_zipped_recursively() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    fs::create_dir(dir.path().join("empty")).unwrap();
    let executable = dir.path().join("nested/file");
    fs::write(&executable, "content").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

    let artifact = zip_context(dir.path()).unwrap();

    assert!(!artifact.bytes.is_empty());
    assert_eq!(artifact.digest.len(), 64);
    let mut archive = zip::ZipArchive::new(Cursor::new(artifact.bytes)).unwrap();
    assert!(archive.by_name("nested/file").is_ok());
    assert!(archive.by_name("empty/").is_ok());
    #[cfg(unix)]
    assert_ne!(
        archive.by_name("nested/file").unwrap().unix_mode().unwrap() & 0o111,
        0
    );
}

#[test]
fn build_uses_fake_aws_and_creates_generic_image() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("source.txt"), "not a Dockerfile").unwrap();
    let fake = FakeAws::start();
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .args([
            "build",
            "-t",
            "demo",
            "--artifact-bucket",
            "bucket",
            "--build-role-arn",
            "arn:role",
            dir.path().to_str().unwrap(),
        ])
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCOUNT_ID", "123456789012")
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
    let requests = fake.finish();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].contains("source.txt") || requests[0].contains("bucket"));
    assert!(
        requests[2].contains("\"name\":\"demo\"") || requests[2].contains("\"name\": \"demo\"")
    );
}

#[test]
fn build_updates_an_existing_image_and_waits_for_completion() {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("source.txt"), "content").unwrap();
    let fake = FakeAws::start_update();

    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .args([
            "build",
            "-t",
            "demo",
            "--artifact-bucket",
            "bucket",
            "--build-role-arn",
            "arn:role",
            directory.path().to_str().unwrap(),
        ])
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCOUNT_ID", "123456789012")
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
    let requests = fake.finish();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].contains("codeArtifact"));
    assert!(requests[3].contains("microvm-image"));
}

#[test]
fn run_embeds_local_script_arguments_and_environment() {
    let directory = TempDir::new().unwrap();
    let script = directory.path().join("job.sh");
    let environment = directory.path().join("runtime.env");
    fs::write(&script, "exec echo \"$TOKEN:$1\"\n").unwrap();
    fs::write(&environment, "TOKEN=literal=value\n").unwrap();
    let fake = FakeAws::start_responses(vec![Response {
        status: 200,
        body: r#"{"microvmId":"microvm-123","state":"PENDING","endpoint":"https://example.test","imageArn":"image","imageVersion":"1","maximumDurationInSeconds":3600,"startedAt":1787616000}"#,
    }]);

    let output = Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .args([
            "run",
            "--execution-role-arn",
            "arn:role",
            "--env-file",
            environment.to_str().unwrap(),
            "--script",
            script.to_str().unwrap(),
            "demo",
            "argument",
            "--literal-option",
        ])
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCOUNT_ID", "123456789012")
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
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "microvm-123"
    );
    let request = fake.finish().pop().unwrap();
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let request: Value = serde_json::from_str(body).unwrap();
    let payload: Value = serde_json::from_str(request["runHookPayload"].as_str().unwrap()).unwrap();
    assert_eq!(payload["command"], "/bin/sh");
    assert_eq!(
        payload["args"],
        serde_json::json!([
            "-c",
            "exec echo \"$TOKEN:$1\"\n",
            "job.sh",
            "argument",
            "--literal-option"
        ])
    );
    assert_eq!(payload["environment"]["TOKEN"], "literal=value");
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
    fn start() -> Self {
        Self::start_responses(vec![
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
                body: "{}",
            },
            Response {
                status: 200,
                body: r#"{"imageArn":"image","name":"demo","state":"CREATED","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"1"}"#,
            },
        ])
    }

    fn start_update() -> Self {
        let image = r#"{"imageArn":"image","name":"demo","state":"CREATED","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"1"}"#;
        let updated = r#"{"imageArn":"image","name":"demo","state":"UPDATED","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
        Self::start_responses(vec![
            Response {
                status: 200,
                body: "{}",
            },
            Response {
                status: 200,
                body: image,
            },
            Response {
                status: 200,
                body: "{}",
            },
            Response {
                status: 200,
                body: updated,
            },
        ])
    }

    fn start_status(status: u16) -> Self {
        Self::start_responses(vec![
            Response {
                status: 200,
                body: "{}",
            },
            Response {
                status,
                body: r#"{"__type":"AccessDeniedException","message":"denied"}"#,
            },
        ])
    }

    fn start_responses(responses: Vec<Response>) -> Self {
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
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
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
