mod support;

use clankervm::{Cli, Command as ClankerCommand};
use clap::Parser;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use support::{
    FakeAws, IMAGE_CREATED, IMAGE_CREATING, LOG_EVENTS, LOG_STREAMS, MICROVMS_NONE,
    MICROVMS_PAGE_RUNNING, MICROVMS_PAGE_TERMINATED, Response, VERSION_ACTIVE, VERSION_PENDING,
    VERSIONS_PAGE_ACTIVE, VERSIONS_PAGE_DELETED,
};
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

/// A `[push]` that releases and then prunes everything but the newest version.
const PRUNING_CONFIG: &str = r#"[push]
artifact-bucket = "bucket"
build-role-arn = "arn:aws:iam::123456789012:role/build"
keep-versions = 1
[run]
execution-role-arn = "arn:aws:iam::123456789012:role/run"
"#;

#[test]
fn help_exposes_release_workflow() {
    let output = run_cli(Path::new("."), &["--help"], "");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in ["init", "push", "status", "list", "run", "logs"] {
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
fn list_takes_filters_a_scope_switch_and_its_own_timeout() {
    assert!(Cli::try_parse_from(["clankervm", "list", "--image-version", "7"]).is_err());
    assert!(Cli::try_parse_from(["clankervm", "list", "--state", "RUNNING", "--all"]).is_err());

    let ClankerCommand::List(list) = parse(&[
        "list",
        "--image",
        "demo",
        "--state",
        "running",
        "--state",
        "PENDING",
        "--timeout",
        "30s",
    ]) else {
        panic!("expected list command");
    };

    assert_eq!(list.image.as_deref(), Some("demo"));
    assert_eq!(list.states, ["running", "PENDING"]);
    assert!(!list.all);
    assert_eq!(list.timeout, std::time::Duration::from_secs(30));

    let ClankerCommand::List(list) = parse(&["list", "--all"]) else {
        panic!("expected list command");
    };
    assert!(list.all);
    assert_eq!(list.timeout, std::time::Duration::from_secs(60));
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
fn list_reports_every_page_and_formats_timestamps() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![
        Response::ok(MICROVMS_PAGE_RUNNING),
        Response::ok(MICROVMS_PAGE_TERMINATED),
    ]);

    let result = run_json(directory.path(), &["--format", "json", "list"], &fake.url());

    let microvms = result["microvms"].as_array().unwrap();
    assert_eq!(
        microvms.len(),
        1,
        "terminated MicroVMs are hidden by default"
    );
    assert_eq!(microvms[0]["microvmId"], "microvm-1");
    assert_eq!(microvms[0]["state"], "RUNNING");
    assert_eq!(
        microvms[0]["imageArn"],
        "arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"
    );
    assert_eq!(microvms[0]["imageVersion"], "7");
    assert_eq!(microvms[0]["startedAt"], "2026-08-25T00:00:00Z");

    let requests = fake.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("maxResults=50"), "{}", requests[0]);
    assert!(!requests[0].contains("imageIdentifier"), "{}", requests[0]);
    assert!(requests[1].contains("nextToken=page-2"), "{}", requests[1]);
}

#[test]
fn list_forwards_the_image_filters() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![Response::ok(MICROVMS_NONE)]);

    let result = run_json(
        directory.path(),
        &[
            "--format",
            "json",
            "list",
            "--image",
            "demo",
            "--image-version",
            "7",
        ],
        &fake.url(),
    );

    assert_eq!(result["microvms"].as_array().unwrap().len(), 0);
    let request = fake.finish().pop().unwrap();
    assert!(request.contains("imageIdentifier=demo"), "{request}");
    assert!(request.contains("imageVersion=7"), "{request}");
}

#[test]
fn list_human_output_names_the_region_and_the_scope() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![Response::ok(MICROVMS_NONE)]);

    let output = run_cli(directory.path(), &["list"], &fake.url());

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Region:  us-east-1"), "{text}");
    assert!(text.contains("Profile: default credential chain"), "{text}");
    assert!(text.contains("No MicroVMs found."), "{text}");
    assert!(text.contains("pass --all to include them"), "{text}");
    fake.finish();
}

#[test]
fn list_rejects_an_image_arn_from_another_region() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");

    let output = run_cli(
        directory.path(),
        &[
            "--format",
            "json",
            "list",
            "--image",
            "arn:aws:lambda:eu-west-1:123456789012:microvm-image:demo",
        ],
        "http://127.0.0.1:1",
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not in region `us-east-1`"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn list_fails_without_reporting_a_partial_page_run() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![
        Response::ok(MICROVMS_PAGE_RUNNING),
        Response::not_found(),
    ]);

    let output = run_cli(directory.path(), &["--format", "json", "list"], &fake.url());

    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    fake.finish();
}

#[test]
fn logs_takes_a_microvm_and_its_own_read_settings() {
    assert!(Cli::try_parse_from(["clankervm", "logs"]).is_err());

    let ClankerCommand::Logs(logs) = parse(&[
        "logs",
        "microvm-1",
        "--follow",
        "--raw",
        "--log-group",
        "/demo/runs",
        "--log-stream",
        "custom",
        "--since",
        "15m",
        "--limit",
        "10",
        "--timeout",
        "5s",
    ]) else {
        panic!("expected logs command");
    };

    assert_eq!(logs.microvm_id, "microvm-1");
    assert!(logs.follow);
    assert!(logs.raw);
    assert_eq!(logs.settings.log_group.as_deref(), Some("/demo/runs"));
    assert_eq!(logs.settings.log_stream.as_deref(), Some("custom"));
    assert_eq!(
        logs.settings.since,
        Some(std::time::Duration::from_mins(15))
    );
    assert_eq!(logs.settings.limit, Some(10));
    assert_eq!(
        logs.settings.timeout,
        Some(std::time::Duration::from_secs(5))
    );
}

#[test]
fn logs_reads_the_stream_of_the_group_run_writes_to() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::ok(LOG_EVENTS)]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "logs", "microvm-1"],
        &fake.url(),
    );

    assert_eq!(result["logGroup"], "/demo/runs");
    assert_eq!(result["logStream"], "microvm-1");
    let events = result["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["timestamp"], "2026-08-25T00:00:01Z");
    assert_eq!(events[0]["message"], "hello from a MicroVM\n");

    let request = fake.finish().pop().unwrap();
    assert!(request.contains("Logs_20140328.GetLogEvents"), "{request}");
    assert!(
        request.contains("\"logGroupName\":\"/demo/runs\""),
        "{request}"
    );
    assert!(
        request.contains("\"logStreamName\":\"microvm-1\""),
        "{request}"
    );
    assert!(request.contains("\"startFromHead\":false"), "{request}");
    assert!(request.contains("\"limit\":1000"), "{request}");
    assert!(!request.contains("nextToken"), "{request}");
}

#[test]
fn logs_human_output_prints_events_with_their_timestamps() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::ok(LOG_EVENTS)]);

    let output = run_cli(directory.path(), &["logs", "microvm-1"], &fake.url());

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "2026-08-25T00:00:01Z  hello from a MicroVM\n"
    );
    fake.finish();
}

#[test]
fn logs_raw_output_prints_the_messages_alone() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::ok(LOG_EVENTS)]);

    let output = run_cli(
        directory.path(),
        &["logs", "microvm-1", "--raw"],
        &fake.url(),
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "hello from a MicroVM\n"
    );
    fake.finish();
}

#[test]
fn logs_uses_the_group_aws_streams_to_without_configuration() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![Response::ok(LOG_EVENTS)]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "logs", "microvm-1"],
        &fake.url(),
    );

    assert_eq!(result["logGroup"], "/aws/lambda-microvms/demo");
    let request = fake.finish().pop().unwrap();
    assert!(
        request.contains("\"logGroupName\":\"/aws/lambda-microvms/demo\""),
        "{request}"
    );
}

#[test]
fn logs_flags_point_at_another_destination() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::ok(LOG_EVENTS)]);

    let result = run_json(
        directory.path(),
        &[
            "--format",
            "json",
            "logs",
            "microvm-1",
            "--log-group",
            "/other/group",
            "--log-stream",
            "custom",
        ],
        &fake.url(),
    );

    assert_eq!(result["logGroup"], "/other/group");
    assert_eq!(result["logStream"], "custom");
    let request = fake.finish().pop().unwrap();
    assert!(
        request.contains("\"logGroupName\":\"/other/group\""),
        "{request}"
    );
    assert!(
        request.contains("\"logStreamName\":\"custom\""),
        "{request}"
    );
}

#[test]
fn logs_names_the_streams_the_group_has_when_the_stream_is_missing() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::not_found(), Response::ok(LOG_STREAMS)]);

    let output = run_cli(directory.path(), &["logs", "microvm-9"], &fake.url());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no log stream `microvm-9` in log group `/demo/runs`"),
        "{stderr}"
    );
    assert!(stderr.contains("the group has: job-1, job-2"), "{stderr}");
    let requests = fake.finish();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert!(
        requests[1].contains("Logs_20140328.DescribeLogStreams"),
        "{}",
        requests[1]
    );
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

#[test]
fn push_prunes_every_page_of_old_versions() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), PRUNING_CONFIG);
    let fake = FakeAws::start(vec![
        Response::ok("{}"),
        Response::not_found(),
        Response::ok(IMAGE_CREATING),
        Response::ok(IMAGE_CREATED),
        Response::ok(VERSION_ACTIVE),
        Response::ok(VERSIONS_PAGE_ACTIVE),
        Response::ok(VERSIONS_PAGE_DELETED),
        Response::ok("{}"),
    ]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "push", "--timeout", "5s"],
        &fake.url(),
    );

    assert_eq!(result["release"], "demo@2");
    let requests = fake.finish();
    assert_eq!(requests.len(), 8, "{requests:#?}");
    assert!(requests[5].contains("maxResults=50"), "{}", requests[5]);
    assert!(
        requests[6].contains("nextToken=versions-2"),
        "{}",
        requests[6]
    );
    let deleted: Vec<&String> = requests
        .iter()
        .filter(|request| request.starts_with("DELETE "))
        .collect();
    assert_eq!(deleted.len(), 1, "{requests:#?}");
    assert!(
        deleted[0].contains("/versions/1"),
        "the active version and the deleted one are skipped: {}",
        deleted[0]
    );
}

#[test]
fn service_failures_report_the_code_and_message_aws_returns() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![Response::access_denied()]);

    let output = run_cli(directory.path(), &["--format", "json", "list"], &fake.url());

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("list MicroVMs failed"), "{stderr}");
    assert!(
        stderr.contains("AccessDeniedException: not allowed to list MicroVMs"),
        "{stderr}"
    );
    fake.finish();
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
        .env("AWS_ENDPOINT_URL_CLOUDWATCH_LOGS", url)
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
