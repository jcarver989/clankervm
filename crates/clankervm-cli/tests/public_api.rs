mod support;

use clankervm::{Cli, Command as ClankerCommand};
use clap::Parser;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use support::{FakeAws, IMAGE_CREATED, IMAGE_CREATING, Response, VERSION_ACTIVE, VERSION_PENDING};
use tempfile::TempDir;

/// A `[push]` and `[run]` configuration with every deployment role set.
const FULL_CONFIG: &str = r#"[push]
artifact-bucket = "bucket"
build-role-arn = "arn:aws:iam::123456789012:role/build"
[run]
command = ["echo", "hello"]
environment = ["GREETING=hello"]
execution-role-arn = "arn:aws:iam::123456789012:role/run"
log-group = "/demo/runs"
"#;

#[test]
fn help_exposes_release_workflow() {
    let output = run_cli(Path::new("."), &["--help"], "");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in ["init", "push", "status", "run"] {
        assert!(text.contains(command), "missing {command} in {text}");
    }
    for removed in ["  bundle", "  wait"] {
        assert!(!text.contains(removed), "unexpected {removed} in {text}");
    }
}

#[test]
fn run_requires_separator_before_the_command() {
    let ClankerCommand::Run(run) = parse(&[
        "run",
        "--execution-role-arn",
        "arn:aws:iam::123456789012:role/demo",
        "--env",
        "GREETING=hello",
        "--",
        "command",
        "--command-option",
    ]) else {
        panic!("expected run command");
    };
    assert_eq!(run.arguments, ["command", "--command-option"]);
    assert_eq!(run.settings.environment.unwrap(), ["GREETING=hello"]);
}

#[test]
fn push_flags_mirror_flat_config_values() {
    let ClankerCommand::Push(push) = parse(&[
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
    ]) else {
        panic!("expected push command");
    };

    let settings = push.settings;
    assert_eq!(settings.context.as_deref(), Some(Path::new("image")));
    assert_eq!(settings.artifact_bucket.as_deref(), Some("artifacts"));
    assert_eq!(settings.capabilities.unwrap(), ["ALL"]);
    assert_eq!(
        settings.tags.unwrap(),
        ["team=platform", "environment=test"]
    );
    assert_eq!(settings.ready_timeout_seconds, Some(120));
}

#[test]
fn push_accepts_a_directory_or_zip_path() {
    for path in ["prepared-directory", "prepared-image.zip"] {
        let ClankerCommand::Push(push) = parse(&["push", path]) else {
            panic!("expected push command");
        };
        assert_eq!(push.source.as_deref(), Some(Path::new(path)));
    }
}

#[test]
fn status_owns_waiting_and_removed_options_are_rejected() {
    let ClankerCommand::Status(status) = parse(&["status", "demo@2", "--wait", "--timeout", "5m"])
    else {
        panic!("expected status command");
    };
    assert!(status.wait);
    assert_eq!(status.release.as_deref(), Some("demo@2"));
    assert_eq!(
        status.settings.timeout,
        Some(std::time::Duration::from_secs(300))
    );

    for args in [
        vec!["bundle"],
        vec!["wait"],
        vec!["push", "--detach"],
        vec!["push", "--bundle", "image.zip"],
        vec!["push", "--image", "agent"],
        vec!["status", "--image", "agent"],
        vec!["run", "--image", "agent", "--", "echo"],
        vec!["push", "--poll-interval", "1s"],
        vec!["push", "--capability", "NOPE"],
        vec!["push", "--timeout", "eventually"],
        vec!["run", "--script", "run.sh", "--", "echo"],
    ] {
        let args: Vec<&str> = [&["clankervm"][..], &args].concat();
        assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn init_only_creates_the_project_file() {
    let directory = TempDir::new().unwrap();
    let output = run_cli(directory.path(), &["init", "--name", "demo"], "");
    assert!(output.status.success());
    let text = fs::read_to_string(directory.path().join("clankervm.toml")).unwrap();
    assert!(text.starts_with("schema-version = 1"), "{text}");
    for expected in [
        "name = \"demo\"",
        "[image]",
        "[push]",
        "# artifact-bucket = ",
        "# build-role-arn = ",
        "[run]",
        "# execution-role-arn = ",
    ] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    for removed in ["[bundle]", "[app]"] {
        assert!(!text.contains(removed), "unexpected {removed} in {text}");
    }
    assert!(!directory.path().join(".gitignore").exists());
    assert!(!directory.path().join(".clankervm").exists());
}

#[test]
fn init_emits_json_when_requested() {
    let directory = TempDir::new().unwrap();
    let json = run_json(
        directory.path(),
        &["--format", "json", "init", "--name", "demo"],
        "",
    );
    assert_eq!(json["configPath"], "clankervm.toml");
}

#[test]
fn status_resolves_the_image_from_the_execution_role_alone() {
    let directory = TempDir::new().unwrap();
    write_config(
        directory.path(),
        "[run]\nexecution-role-arn = \"arn:aws:iam::123456789012:role/run\"\n",
    );
    let fake = FakeAws::start(vec![
        Response::ok(IMAGE_CREATED),
        Response::ok(VERSION_ACTIVE),
    ]);
    let result = run_json(
        directory.path(),
        &["--format", "json", "status"],
        &fake.url(),
    );
    assert_eq!(result["release"], "demo@2");
    assert_eq!(
        result["imageArn"],
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
    );
    fake.finish();
}

#[test]
fn push_waits_for_the_exact_version_to_become_active() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    fs::write(directory.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let fake = FakeAws::start(vec![
        Response::ok("{}"),
        Response::not_found(),
        Response::ok(IMAGE_CREATING),
        Response::ok(IMAGE_CREATED),
        Response::ok(VERSION_PENDING),
        Response::ok(IMAGE_CREATED),
        Response::ok(VERSION_ACTIVE),
    ]);
    let result = run_json(
        directory.path(),
        &["--format", "json", "push", "--timeout", "5s"],
        &fake.url(),
    );
    assert_eq!(result["release"], "demo@2");
    assert_eq!(result["versionState"], "SUCCESSFUL");
    assert_eq!(result["versionStatus"], "ACTIVE");
    let requests = fake.finish();
    assert_eq!(requests.len(), 7);
    assert!(requests[0].contains("clankervm/demo/bundles/"));
    assert!(requests[0].contains("Dockerfile"));
}

#[test]
fn run_uses_project_defaults_and_forwards_client_token() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::ok(
        r#"{"microvmId":"microvm-123","state":"PENDING","endpoint":"https://example.test","imageArn":"image","imageVersion":"7","maximumDurationInSeconds":3600,"startedAt":1787616000}"#,
    )]);
    let result = run_json(
        directory.path(),
        &["--format", "json", "run", "--client-token", "run-42"],
        &fake.url(),
    );
    assert_eq!(result["microvmId"], "microvm-123");
    assert_eq!(result["imageVersion"], "7");
    let request = fake.finish().pop().unwrap();
    for expected in ["run-42", "echo", "hello", "GREETING"] {
        assert!(
            request.contains(expected),
            "missing {expected} in {request}"
        );
    }
}

fn parse(args: &[&str]) -> ClankerCommand {
    Cli::try_parse_from([&["clankervm"][..], args].concat())
        .unwrap()
        .command
}

/// Runs the binary in `directory` against the fake AWS endpoint at `url`.
fn run_cli(directory: &Path, args: &[&str], url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clankervm"))
        .current_dir(directory)
        .args(args)
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_ENDPOINT_URL", url)
        .env("AWS_ENDPOINT_URL_LAMBDA_MICROVMS", url)
        .output()
        .unwrap()
}

/// Runs the binary, asserts success, and parses its JSON output.
fn run_json(directory: &Path, args: &[&str], url: &str) -> Value {
    let output = run_cli(directory, args, url);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_config(directory: &Path, sections: &str) {
    fs::write(
        directory.join("clankervm.toml"),
        format!("schema-version = 1\n[image]\nname = \"demo\"\nregion = \"us-east-1\"\n{sections}"),
    )
    .unwrap();
}
