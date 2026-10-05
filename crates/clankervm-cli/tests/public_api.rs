mod support;

use clankervm::{Cli, Command as ClankerCommand};
use clap::Parser;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;
use support::{
    BUILDS_FAILED, FakeAws, IMAGE_CREATE_FAILED, IMAGE_CREATED, IMAGE_CREATING, IMAGE_UPDATED,
    IMAGE_UPDATING, LOG_EVENTS, LOG_STREAMS, MICROVMS_NONE, MICROVMS_PAGE_RUNNING,
    MICROVMS_PAGE_TERMINATED, Response, VERSION_3_ACTIVE, VERSION_ACTIVE, VERSION_FAILED,
    VERSION_PENDING, VERSIONS_PAGE_ACTIVE, VERSIONS_PAGE_DELETED,
};
use tempfile::TempDir;

const LOG_MICROVM_ID: &str = "microvm-f4e3b5a1-3a16-3f63-8470-251708859820";
const LOG_STREAM: &str = "2026/09/11[13.0]microvm-f4e3b5a1-3a16-3f63-8470-251708859820";

/// An image and run configuration with every deployment role set.
const FULL_CONFIG: &str = r#"[microvm.image]
iam-role = "arn:aws:iam::123456789012:role/build"
[microvm.image.artifact]
s3-bucket = "bucket"
[microvm.run]
command = ["echo", "hello"]
environment = ["GREETING=hello"]
iam-role = "arn:aws:iam::123456789012:role/run"
[microvm.run.logs]
group = "/demo/runs"
"#;

/// An image that prunes everything but its active version before each release.
const PRUNING_CONFIG: &str = r#"[microvm.image]
iam-role = "arn:aws:iam::123456789012:role/build"
[microvm.image.artifact]
s3-bucket = "bucket"
[microvm.image.versions]
max = 1
[microvm.run]
iam-role = "arn:aws:iam::123456789012:role/run"
"#;

#[test]
fn help_exposes_release_workflow() {
    let output = run_cli(Path::new("."), &["--help"], "");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in [
        "init", "push", "status", "prune", "list", "run", "logs", "shell", "connect", "describe",
        "suspend", "resume", "stop",
    ] {
        assert!(text.contains(command), "missing {command} in {text}");
    }
    for removed in ["  bundle", "  wait"] {
        assert!(!text.contains(removed), "unexpected {removed} in {text}");
    }
}

#[test]
fn lifecycle_commands_parse_wait_and_positive_timeouts() {
    let ClankerCommand::Suspend(suspend) = parse(&["suspend", "vm-1"]) else {
        panic!("expected suspend command");
    };
    assert_eq!(suspend.vm_id, "vm-1");
    assert!(!suspend.wait);
    assert_eq!(suspend.timeout, Duration::from_secs(30));

    let ClankerCommand::Resume(resume) = parse(&["resume", "vm-1", "--wait", "--timeout", "2m"])
    else {
        panic!("expected resume command");
    };
    assert_eq!(resume.vm_id, "vm-1");
    assert!(resume.wait);
    assert_eq!(resume.timeout, Duration::from_secs(120));

    for command in ["suspend", "resume"] {
        assert!(Cli::try_parse_from(["clankervm", command, "vm-1", "--timeout", "0s"]).is_err());
    }
}

#[test]
fn shell_attaches_or_launches() {
    let ClankerCommand::Shell(shell) = parse(&["shell"]) else {
        panic!("expected shell command");
    };
    assert!(shell.microvm_id.is_none());
    assert!(!shell.keep);
    assert_eq!(shell.timeout, std::time::Duration::from_mins(5));
    assert!(shell.run.arguments.is_empty());
    assert!(shell.run.release.is_none());
    assert!(shell.run.settings.ingress.is_none());

    let ClankerCommand::Shell(shell) = parse(&["shell", "microvm-1", "--timeout", "10m"]) else {
        panic!("expected shell command");
    };
    assert_eq!(shell.microvm_id.as_deref(), Some("microvm-1"));
    assert_eq!(shell.timeout, std::time::Duration::from_mins(10));

    let ClankerCommand::Shell(shell) = parse(&[
        "shell",
        "--keep",
        "--timeout",
        "30s",
        "--",
        "sleep",
        "infinity",
    ]) else {
        panic!("expected shell command");
    };
    assert!(shell.keep);
    assert_eq!(shell.timeout, std::time::Duration::from_secs(30));
    assert_eq!(shell.run.arguments, ["sleep", "infinity"]);

    // Attaching to an existing MicroVM leaves nothing to launch.
    for rejected in [
        ["shell", "microvm-1", "--keep"].as_slice(),
        &["shell", "microvm-1", "--max-duration", "60"],
        &["shell", "microvm-1", "--release", "my-runner@1"],
        &["shell", "microvm-1", "--", "htop"],
    ] {
        let arguments = std::iter::once("clankervm").chain(rejected.iter().copied());
        assert!(
            Cli::try_parse_from(arguments).is_err(),
            "accepted {rejected:?}"
        );
    }
}

#[test]
fn shell_needs_a_terminal_before_it_reaches_the_project() {
    let directory = TempDir::new().unwrap();

    let output = run_cli(
        directory.path(),
        &["shell", "microvm-1"],
        "http://127.0.0.1:1",
    );

    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("interactive terminal"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn shell_rejects_json_before_project_or_credential_setup() {
    let directory = TempDir::new().unwrap();
    for arguments in [
        vec!["--format", "json", "shell"],
        vec!["--format", "json", "shell", "microvm-1"],
    ] {
        let output = run_cli(directory.path(), &arguments, "http://127.0.0.1:1");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("cannot be combined with --format json"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn named_connections_separate_remote_and_local_arguments() {
    let ClankerCommand::Run(options) = parse(&[
        "run",
        "--connect",
        "aether",
        "--connect-timeout",
        "1m",
        "--",
        "/remote-only",
        "--agent",
        "Simple Builder",
    ]) else {
        panic!("expected run");
    };
    assert_eq!(options.connect.as_deref(), Some("aether"));
    assert_eq!(
        options.launch.arguments,
        ["/remote-only", "--agent", "Simple Builder"]
    );
    let ClankerCommand::Connect(options) = parse(&[
        "connect",
        "vm-1",
        "aether",
        "--",
        "--session",
        "a b",
        "",
        "x=y",
    ]) else {
        panic!("expected connect");
    };
    assert_eq!(options.arguments, ["--session", "a b", "", "x=y"]);
    for args in [
        vec!["run", "--connect-timeout", "1s"],
        vec!["run", "--connect", "aether", "--client-token", "replay"],
        vec!["connect", "vm-1"],
        vec!["shell", "--connect", "aether"],
        vec!["stop", "vm-1", "--timeout", "0s"],
    ] {
        assert!(Cli::try_parse_from(std::iter::once("clankervm").chain(args)).is_err());
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
    assert_eq!(run.launch.arguments, ["command", "--command-option"]);
    assert_eq!(run.launch.settings.environment.unwrap(), ["GREETING=hello"]);
}

#[test]
fn push_flags_populate_command_settings() {
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
fn image_busy_timeout_is_a_global_option() {
    let default = Cli::try_parse_from(["clankervm", "push"]).unwrap();
    assert_eq!(
        default.image_busy_timeout, None,
        "unset so the project file applies"
    );

    for args in [
        ["clankervm", "--image-busy-timeout", "90s", "push"],
        ["clankervm", "push", "--image-busy-timeout", "90s"],
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(
            cli.image_busy_timeout,
            Some(Duration::from_secs(90)),
            "{args:?}"
        );
    }
    assert!(Cli::try_parse_from(["clankervm", "--image-busy-timeout", "soon", "push"]).is_err());
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
        vec!["prune", "--keep-versions", "-1"],
        vec!["prune", "--keep", "3"],
        vec!["prune", "demo@2"],
    ] {
        let args: Vec<&str> = [&["clankervm"][..], &args].concat();
        assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn prune_takes_a_number_of_versions_to_keep() {
    let ClankerCommand::Prune(prune) = parse(&["prune", "--keep-versions", "3"]) else {
        panic!("expected prune command");
    };
    assert_eq!(prune.settings.keep_versions, Some(3));

    let ClankerCommand::Prune(prune) = parse(&["prune"]) else {
        panic!("expected prune command");
    };
    assert_eq!(prune.settings.keep_versions, None);
}

#[test]
fn init_only_creates_the_project_file() {
    let directory = TempDir::new().unwrap();
    let output = run_cli(directory.path(), &["init", "--name", "demo"], "");
    assert!(output.status.success());
    let text = fs::read_to_string(directory.path().join("clankervm.toml")).unwrap();
    assert!(text.starts_with("[aws]"), "{text}");
    for expected in [
        "name = \"demo\"",
        "[microvm]",
        "[microvm.image]",
        "[microvm.image.artifact]",
        "# s3-bucket = ",
        "[microvm.run]",
        "# iam-role = ",
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
        "[microvm.run]\niam-role = \"arn:aws:iam::123456789012:role/run\"\n",
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
fn push_sends_image_hook_commands_on_create_update_and_removal() {
    for (existing, configured, override_timeout) in [
        (false, true, false),
        (true, true, true),
        (true, false, false),
    ] {
        let directory = TempDir::new().unwrap();
        let mut config = FULL_CONFIG.to_owned();
        config.push_str("\n[microvm.image.hooks]\nvalidate-timeout = '7m'\n");
        if configured {
            config.push_str("\n[microvm.image.hooks.ready]\ncommand = ['echo', 'hello world']\nenvironment = ['WORKSPACE=/workspace/repo']\n[microvm.image.hooks.validate]\ncommand = ['echo', 'validate workspace']\nenvironment = ['WORKSPACE=/workspace/repo']\n");
        }
        write_config(directory.path(), &config);
        fs::write(directory.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let image = if existing {
            Response::ok(IMAGE_CREATED)
        } else {
            Response::not_found()
        };
        let fake = FakeAws::start(vec![
            Response::ok("{}"),
            image,
            Response::ok(IMAGE_CREATING),
            Response::ok(IMAGE_CREATED),
            Response::ok(VERSION_ACTIVE),
        ]);
        let mut args = vec!["--format", "json", "push", "--timeout", "5s"];
        if override_timeout {
            args.extend(["--validate-timeout-seconds", "600"]);
        }
        run_json(directory.path(), &args, &fake.url());
        let requests = fake.finish();
        let request = &requests[2];
        let method = if existing { "PUT " } else { "POST " };
        assert!(request.starts_with(method), "{request}");
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        if configured {
            let payload: Value = serde_json::from_str(
                body["environmentVariables"]["CLANKERVM_READY_HOOK_PAYLOAD"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(payload["command"], "echo");
            assert_eq!(payload["args"], serde_json::json!(["hello world"]));
            assert_eq!(payload["environment"]["WORKSPACE"], "/workspace/repo");
            assert_eq!(payload["environment"]["AWS_REGION"], "us-east-1");
            let validate: Value = serde_json::from_str(
                body["environmentVariables"]["CLANKERVM_VALIDATE_HOOK_PAYLOAD"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(validate["command"], "echo");
            assert_eq!(validate["args"], serde_json::json!(["validate workspace"]));
            assert_eq!(validate["environment"]["WORKSPACE"], "/workspace/repo");
            assert_eq!(validate["environment"]["AWS_REGION"], "us-east-1");
            assert_eq!(validate["environment"]["AWS_DEFAULT_REGION"], "us-east-1");
            assert_eq!(body["hooks"]["microvmImageHooks"]["validate"], "ENABLED");
            assert_eq!(
                body["hooks"]["microvmImageHooks"]["validateTimeoutInSeconds"],
                if override_timeout { 600 } else { 420 }
            );
        } else {
            assert_eq!(body["hooks"]["microvmImageHooks"]["validate"], "DISABLED");
            assert!(
                body["hooks"]["microvmImageHooks"]
                    .get("validateTimeoutInSeconds")
                    .is_none()
            );
            assert_eq!(body["environmentVariables"], serde_json::json!({}));
        }
    }
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
    for invalid in [
        LOG_STREAM,
        "microvm-1",
        "",
        "f4e3b5a1-3a16-3f63-8470-251708859820",
    ] {
        assert!(Cli::try_parse_from(["clankervm", "logs", invalid]).is_err());
    }
    assert!(
        Cli::try_parse_from([
            "clankervm",
            "logs",
            LOG_MICROVM_ID,
            "--log-stream",
            "custom"
        ])
        .is_err()
    );

    let ClankerCommand::Logs(logs) = parse(&[
        "logs",
        LOG_MICROVM_ID,
        "--follow",
        "--raw",
        "--log-group",
        "/demo/runs",
        "--since",
        "15m",
        "--limit",
        "10",
    ]) else {
        panic!("expected logs command");
    };

    assert_eq!(logs.microvm_id, LOG_MICROVM_ID);
    assert!(logs.follow);
    assert!(logs.raw);
    assert_eq!(logs.settings.log_group.as_deref(), Some("/demo/runs"));
    assert_eq!(
        logs.settings.since,
        Some(std::time::Duration::from_mins(15))
    );
    assert_eq!(logs.settings.limit, Some(10));
    assert!(Cli::try_parse_from(["clankervm", "logs", LOG_MICROVM_ID, "--timeout", "5s"]).is_err());
}

#[test]
fn logs_reads_the_stream_of_the_group_run_writes_to() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![Response::ok(LOG_STREAMS), Response::ok(LOG_EVENTS)]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "logs", LOG_MICROVM_ID],
        &fake.url(),
    );

    assert_eq!(result["logGroup"], "/demo/runs");
    assert_eq!(result["logStream"], LOG_STREAM);
    let events = result["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["timestamp"], "2026-08-25T00:00:01Z");
    assert_eq!(events[0]["message"], "hello from a MicroVM\n");

    let requests = fake.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("Logs_20140328.DescribeLogStreams"));
    assert!(requests[0].contains("\"logGroupName\":\"/demo/runs\""));
    let request = &requests[1];
    assert!(request.contains("Logs_20140328.GetLogEvents"), "{request}");
    assert!(
        request.contains("\"logGroupName\":\"/demo/runs\""),
        "{request}"
    );
    assert!(
        request.contains(&format!("\"logStreamName\":\"{LOG_STREAM}\"")),
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
    let fake = FakeAws::start(vec![Response::ok(LOG_STREAMS), Response::ok(LOG_EVENTS)]);

    let output = run_cli(directory.path(), &["logs", LOG_MICROVM_ID], &fake.url());

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
    let fake = FakeAws::start(vec![Response::ok(LOG_STREAMS), Response::ok(LOG_EVENTS)]);

    let output = run_cli(
        directory.path(),
        &["logs", LOG_MICROVM_ID, "--raw"],
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
    let fake = FakeAws::start(vec![Response::ok(LOG_STREAMS), Response::ok(LOG_EVENTS)]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "logs", LOG_MICROVM_ID],
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
    let fake = FakeAws::start(vec![Response::ok(LOG_STREAMS), Response::ok(LOG_EVENTS)]);

    let result = run_json(
        directory.path(),
        &[
            "--format",
            "json",
            "logs",
            LOG_MICROVM_ID,
            "--log-group",
            "/other/group",
        ],
        &fake.url(),
    );

    assert_eq!(result["logGroup"], "/other/group");
    assert_eq!(result["logStream"], LOG_STREAM);
    let request = fake.finish().pop().unwrap();
    assert!(
        request.contains("\"logGroupName\":\"/other/group\""),
        "{request}"
    );
    assert!(
        request.contains(&format!("\"logStreamName\":\"{LOG_STREAM}\"")),
        "{request}"
    );
}

#[test]
fn logs_reports_the_id_and_group_when_no_stream_matches() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    for arguments in [
        ["logs", LOG_MICROVM_ID].as_slice(),
        &["logs", LOG_MICROVM_ID, "--follow"],
    ] {
        let fake = FakeAws::start(vec![Response::ok(r#"{"logStreams":[]}"#)]);
        let output = run_cli(directory.path(), arguments, &fake.url());

        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "no log stream for MicroVM `{LOG_MICROVM_ID}` in log group `/demo/runs`"
            )),
            "{stderr}"
        );
        fake.finish();
    }
}

#[test]
fn logs_discovers_a_match_after_an_empty_page() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![
        Response::ok(r#"{"logStreams":[],"nextToken":"streams-2"}"#),
        Response::ok(LOG_STREAMS),
        Response::ok(LOG_EVENTS),
    ]);
    let result = run_json(
        directory.path(),
        &["--format", "json", "logs", LOG_MICROVM_ID],
        &fake.url(),
    );
    assert_eq!(result["logStream"], LOG_STREAM);
    let requests = fake.finish();
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert!(request.contains("Logs_20140328.DescribeLogStreams"));
        assert!(request.contains("\"logGroupName\":\"/demo/runs\""));
    }
    assert!(!requests[0].contains("nextToken"));
    assert!(requests[1].contains("\"nextToken\":\"streams-2\""));
    assert!(requests[2].contains("Logs_20140328.GetLogEvents"));
}

#[test]
fn logs_checks_later_pages_for_ambiguity_before_reading() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![
        Response::ok(
            r#"{"logStreams":[{"logStreamName":"2026/09/10[12.0]microvm-f4e3b5a1-3a16-3f63-8470-251708859820"}],"nextToken":"streams-2"}"#,
        ),
        Response::ok(LOG_STREAMS),
    ]);
    let output = run_cli(directory.path(), &["logs", LOG_MICROVM_ID], &fake.url());
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("multiple log streams"), "{stderr}");
    assert!(stderr.contains(LOG_STREAM), "{stderr}");
    assert!(stderr.contains("2026/09/10[12.0]"), "{stderr}");
    let requests = fake.finish();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.contains("Logs_20140328.DescribeLogStreams"))
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
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(body.get("idlePolicy").is_none(), "{body}");
}

#[test]
fn push_prunes_every_page_of_old_versions_before_publishing() {
    let requests = Pruning::push(vec![
        Response::ok(VERSIONS_PAGE_ACTIVE),
        Response::ok(VERSIONS_PAGE_DELETED),
        Response::ok(MICROVMS_NONE),
        Response::ok("{}"),
    ])
    .succeeds();

    assert_eq!(requests.len(), 9, "{requests:#?}");
    assert!(requests[0].contains("maxResults=50"), "{}", requests[0]);
    assert!(
        requests[1].contains("nextToken=versions-2"),
        "{}",
        requests[1]
    );
    assert!(requests[3].starts_with("DELETE "), "{}", requests[3]);
    let deleted = sent(&requests, &["DELETE "]);
    assert_eq!(deleted.len(), 1, "{requests:#?}");
    assert!(
        deleted[0].contains("/versions/1"),
        "the newest launchable version and the deleted one are skipped: {}",
        deleted[0]
    );
}

#[test]
fn push_retries_pruning_while_the_image_is_updating() {
    let requests = Pruning::push(vec![
        Response::ok(VERSIONS_PAGE_ACTIVE),
        Response::ok(VERSIONS_PAGE_DELETED),
        Response::ok(MICROVMS_NONE),
        Response::image_updating(),
        Response::ok("{}"),
    ])
    .succeeds();

    let deleted = sent(&requests, &["DELETE "]);
    assert_eq!(deleted.len(), 2, "{requests:#?}");
    assert!(
        deleted
            .iter()
            .all(|request| request.contains("/versions/1")),
        "{deleted:#?}"
    );
}

/// Runs a command that prunes old image versions against scripted AWS
/// responses for the pruning it does.
struct Pruning {
    args: Vec<&'static str>,
    max: usize,
    responses: Vec<Response>,
}

impl Pruning {
    /// `push`, which prunes before it publishes a new version.
    fn push(responses: Vec<Response>) -> Self {
        Self {
            args: vec!["push", "--timeout", "5s"],
            max: 1,
            responses,
        }
    }

    /// The standalone `prune` command.
    fn prune(responses: Vec<Response>) -> Self {
        Self {
            args: vec!["prune"],
            max: 1,
            responses,
        }
    }

    fn max(mut self, max: usize) -> Self {
        self.max = max;
        self
    }

    fn succeeds(mut self) -> Vec<String> {
        self.responses.extend([
            Response::ok("{}"),
            Response::ok(IMAGE_CREATED),
            Response::ok(IMAGE_UPDATING),
            Response::ok(IMAGE_UPDATED),
            Response::ok(VERSION_3_ACTIVE),
        ]);
        let (result, requests) = self.reports();
        assert_eq!(result["release"], "demo@3");
        requests
    }

    fn reports(self) -> (Value, Vec<String>) {
        let (output, fake) = self.run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            serde_json::from_slice(&output.stdout).unwrap(),
            fake.finish(),
        )
    }

    fn fails(self, error: &str) -> Vec<String> {
        let (output, fake) = self.run();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains(error), "expected {error:?} in {stderr}");
        fake.finish()
    }

    fn run(self) -> (Output, FakeAws) {
        let directory = TempDir::new().unwrap();
        write_config(
            directory.path(),
            &PRUNING_CONFIG.replace("max = 1", &format!("max = {}", self.max)),
        );
        let fake = FakeAws::start(self.responses);
        let args = [&["--format", "json"][..], &self.args].concat();
        let output = run_cli(directory.path(), &args, &fake.url());
        (output, fake)
    }
}

fn sent<'a>(requests: &'a [String], methods: &[&str]) -> Vec<&'a String> {
    requests
        .iter()
        .filter(|request| methods.iter().any(|method| request.starts_with(method)))
        .collect()
}

fn pruning_versions(old_state: &str, old_status: &str) -> Response {
    let mut page: Value = serde_json::from_str(VERSIONS_PAGE_ACTIVE).unwrap();
    page.as_object_mut().unwrap().remove("nextToken");
    page["items"][1]["state"] = old_state.into();
    page["items"][1]["status"] = old_status.into();
    Response::ok(page.to_string())
}

fn pruning_microvms(state: &str, next_token: Option<&str>) -> Response {
    let mut page: Value = serde_json::from_str(MICROVMS_PAGE_TERMINATED).unwrap();
    page["items"][0]["imageVersion"] = "1".into();
    page["items"][0]["state"] = state.into();
    if let Some(token) = next_token {
        page["nextToken"] = token.into();
    }
    Response::ok(page.to_string())
}

fn pruning_deactivated() -> Response {
    let mut version: Value = serde_json::from_str(VERSION_ACTIVE).unwrap();
    version["imageVersion"] = "1".into();
    version["status"] = "INACTIVE".into();
    Response::ok(version.to_string())
}

#[test]
fn push_deactivates_and_prunes_old_active_versions_with_only_terminated_instances() {
    let requests = Pruning::push(vec![
        pruning_versions("SUCCESSFUL", "ACTIVE"),
        pruning_microvms("TERMINATED", None),
        pruning_deactivated(),
        pruning_microvms("TERMINATED", None),
        Response::ok("{}"),
    ])
    .succeeds();
    assert!(requests[1].contains("imageVersion=1"), "{requests:#?}");
    assert!(requests[1].contains("imageIdentifier="), "{requests:#?}");
    assert!(requests[2].starts_with("PATCH "), "{requests:#?}");
    assert!(requests[2].contains("/versions/1"), "{requests:#?}");
    assert!(
        requests[2].contains(r#""status":"INACTIVE""#),
        "{requests:#?}"
    );
    assert!(requests[3].contains("imageVersion=1"), "{requests:#?}");
    assert!(requests[4].starts_with("DELETE "), "{requests:#?}");
    assert!(requests[4].contains("/versions/1"), "{requests:#?}");
}

#[test]
fn push_preserves_versions_referenced_by_every_non_terminated_state() {
    for state in [
        "PENDING",
        "RUNNING",
        "SUSPENDED",
        "SUSPENDING",
        "TERMINATING",
        "RESUMING",
        "FUTURE_STATE",
    ] {
        let requests = Pruning::push(vec![
            pruning_versions("SUCCESSFUL", "ACTIVE"),
            pruning_microvms("TERMINATED", Some("instances-2")),
            pruning_microvms(state, None),
        ])
        .succeeds();
        assert!(
            requests[2].contains("nextToken=instances-2"),
            "{requests:#?}"
        );
        assert!(
            sent(&requests, &["DELETE ", "PATCH "]).is_empty(),
            "{state}: {requests:#?}"
        );
    }
}

#[test]
fn push_rechecks_all_instance_pages_after_deactivation_and_preserves_a_late_launch() {
    let requests = Pruning::push(vec![
        pruning_versions("SUCCESSFUL", "ACTIVE"),
        Response::ok(MICROVMS_NONE),
        pruning_deactivated(),
        pruning_microvms("TERMINATED", Some("instances-2")),
        pruning_microvms("PENDING", None),
    ])
    .succeeds();
    assert!(requests[2].starts_with("PATCH "), "{requests:#?}");
    assert!(
        requests[4].contains("nextToken=instances-2"),
        "{requests:#?}"
    );
    assert!(sent(&requests, &["DELETE "]).is_empty(), "{requests:#?}");
}

#[test]
fn push_does_not_prune_versions_still_building_or_in_unknown_states() {
    for (state, status) in [
        ("PENDING", "INACTIVE"),
        ("IN_PROGRESS", "INACTIVE"),
        ("FUTURE_STATE", "INACTIVE"),
        ("DELETING", "INACTIVE"),
        ("DELETED", "INACTIVE"),
        ("SUCCESSFUL", "FUTURE_STATUS"),
    ] {
        let requests = Pruning::push(vec![pruning_versions(state, status)]).succeeds();
        assert!(
            sent(&requests, &["DELETE ", "PATCH "]).is_empty(),
            "{state}/{status}: {requests:#?}"
        );
    }
}

#[test]
fn push_keeps_successful_rollback_versions_even_when_a_newer_build_failed() {
    let mut page: Value = serde_json::from_str(VERSIONS_PAGE_ACTIVE).unwrap();
    page.as_object_mut().unwrap().remove("nextToken");
    let mut failed = page["items"][0].clone();
    failed["imageVersion"] = "3".into();
    failed["state"] = "FAILED".into();
    failed["status"] = "INACTIVE".into();
    failed["createdAt"] = 1_787_616_100.into();
    page["items"][1]["status"] = "ACTIVE".into();
    page["items"].as_array_mut().unwrap().push(failed);

    let requests = Pruning::push(vec![
        Response::ok(page.to_string()),
        Response::ok(MICROVMS_NONE),
        Response::ok("{}"),
    ])
    .max(3)
    .succeeds();

    let deleted = sent(&requests, &["DELETE "]);
    assert_eq!(deleted.len(), 1, "{requests:#?}");
    assert!(deleted[0].contains("/versions/3"), "{requests:#?}");
    assert!(sent(&requests, &["PATCH "]).is_empty(), "{requests:#?}");
}

#[test]
fn push_pruning_fails_closed_on_reference_check_or_deactivation_errors() {
    for (responses, error) in [
        (
            vec![
                pruning_versions("SUCCESSFUL", "ACTIVE"),
                Response::access_denied(),
            ],
            "list MicroVMs before pruning failed",
        ),
        (
            vec![
                pruning_versions("SUCCESSFUL", "ACTIVE"),
                Response::ok(MICROVMS_NONE),
                Response::access_denied(),
            ],
            "deactivate image version failed",
        ),
        (
            vec![
                pruning_versions("SUCCESSFUL", "ACTIVE"),
                Response::ok(MICROVMS_NONE),
                pruning_deactivated(),
                Response::access_denied(),
            ],
            "list MicroVMs before pruning failed",
        ),
        (
            vec![
                pruning_versions("SUCCESSFUL", "ACTIVE"),
                Response::ok(MICROVMS_NONE),
                Response::ok(VERSION_ACTIVE),
            ],
            "version 1 is not INACTIVE",
        ),
    ] {
        let requests = Pruning::push(responses).fails(error);
        assert!(
            sent(&requests, &["DELETE ", "POST "]).is_empty(),
            "{requests:#?}"
        );
    }
}

#[test]
fn push_prunes_nothing_before_the_first_release() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), PRUNING_CONFIG);
    let fake = FakeAws::start(vec![
        Response::not_found(),
        Response::ok("{}"),
        Response::not_found(),
        Response::ok(IMAGE_CREATING),
        Response::ok(IMAGE_CREATED),
        Response::ok(VERSION_ACTIVE),
    ]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "push", "--timeout", "5s"],
        &fake.url(),
    );

    assert_eq!(result["release"], "demo@2");
    let requests = fake.finish();
    assert!(requests[0].contains("/versions"), "{}", requests[0]);
    assert!(requests[3].starts_with("POST "), "{}", requests[3]);
}

#[test]
fn prune_keeps_as_many_launchable_versions_as_the_project_allows() {
    let (result, requests) = Pruning::prune(vec![pruning_versions("SUCCESSFUL", "ACTIVE")])
        .max(2)
        .reports();

    assert_eq!(
        requests.len(),
        1,
        "unlike push, prune sets no version aside for a new one: {requests:#?}"
    );
    assert_eq!(result["keepVersions"], 2);
    assert_eq!(result["kept"], json!(["2", "1"]));
    assert_eq!(result["deleted"], json!([]));
}

#[test]
fn prune_deactivates_and_deletes_versions_no_microvm_uses() {
    let (result, requests) = Pruning::prune(vec![
        pruning_versions("SUCCESSFUL", "ACTIVE"),
        pruning_microvms("TERMINATED", None),
        pruning_deactivated(),
        pruning_microvms("TERMINATED", None),
        Response::ok("{}"),
    ])
    .reports();

    assert_eq!(result["kept"], json!(["2"]));
    assert_eq!(result["deleted"], json!(["1"]));
    assert!(requests[2].starts_with("PATCH "), "{requests:#?}");
    assert!(requests[4].starts_with("DELETE "), "{requests:#?}");
    assert!(requests[4].contains("/versions/1"), "{requests:#?}");
}

#[test]
fn prune_reports_a_version_left_inactive_by_a_late_launch() {
    let (result, requests) = Pruning::prune(vec![
        pruning_versions("SUCCESSFUL", "ACTIVE"),
        Response::ok(MICROVMS_NONE),
        pruning_deactivated(),
        pruning_microvms("PENDING", None),
    ])
    .reports();

    assert_eq!(result["deleted"], json!([]));
    assert_eq!(result["inUse"], json!(["1"]));
    assert_eq!(result["deactivated"], json!(["1"]));
    assert!(sent(&requests, &["DELETE "]).is_empty(), "{requests:#?}");
}

#[test]
fn prune_fails_when_the_image_does_not_exist() {
    let requests =
        Pruning::prune(vec![Response::not_found()]).fails("does not exist; push a release first");

    assert_eq!(requests.len(), 1, "{requests:#?}");
}

#[test]
fn prune_needs_a_number_of_versions_to_keep_before_reaching_aws() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);

    let output = run_cli(directory.path(), &["prune"], "");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("microvm.image.versions.max or --keep-versions must be configured"),
        "{stderr}"
    );
}

#[test]
fn push_retries_the_update_while_the_image_is_busy() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![
        Response::ok("{}"),
        Response::ok(IMAGE_CREATED),
        Response::image_already_updating(),
        Response::image_in_current_state(),
        Response::ok(IMAGE_UPDATING),
        Response::ok(IMAGE_UPDATED),
        Response::ok(VERSION_3_ACTIVE),
    ]);

    let result = run_json(
        directory.path(),
        &["--format", "json", "push", "--timeout", "5s"],
        &fake.url(),
    );

    assert_eq!(result["release"], "demo@3");
    let updates = fake
        .finish()
        .iter()
        .filter(|request| request.starts_with("PUT /2025-09-09/microvm-images/"))
        .count();
    assert_eq!(updates, 3);
}

#[test]
fn push_fails_on_a_busy_image_without_an_image_busy_timeout() {
    let from_file = FULL_CONFIG.replace(
        "[microvm.image]\n",
        "[microvm.image]\nbusy-timeout = '0s'\n",
    );
    for (config, flags) in [
        (FULL_CONFIG.to_owned(), &["--image-busy-timeout", "0s"][..]),
        (from_file, &[][..]),
    ] {
        let directory = TempDir::new().unwrap();
        write_config(directory.path(), &config);
        let fake = FakeAws::start(vec![
            Response::ok("{}"),
            Response::ok(IMAGE_CREATED),
            Response::image_already_updating(),
        ]);

        let output = run_cli(
            directory.path(),
            &[flags, &["--format", "json", "push"]].concat(),
            &fake.url(),
        );

        assert!(!output.status.success(), "{flags:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(
                "update image failed: ConflictException: MicroVM Image is already in state: UPDATING"
            ),
            "{stderr}"
        );
        assert_eq!(fake.finish().len(), 3);
    }
}

#[test]
fn image_busy_timeout_flag_overrides_the_project_file() {
    let directory = TempDir::new().unwrap();
    write_config(
        directory.path(),
        &FULL_CONFIG.replace(
            "[microvm.image]\n",
            "[microvm.image]\nbusy-timeout = '0s'\n",
        ),
    );
    let fake = FakeAws::start(vec![
        Response::ok("{}"),
        Response::ok(IMAGE_CREATED),
        Response::image_already_updating(),
        Response::ok(IMAGE_UPDATING),
        Response::ok(IMAGE_UPDATED),
        Response::ok(VERSION_3_ACTIVE),
    ]);

    let result = run_json(
        directory.path(),
        &["--image-busy-timeout", "1m", "--format", "json", "push"],
        &fake.url(),
    );

    assert_eq!(result["release"], "demo@3");
    fake.finish();
}

#[test]
fn push_reports_the_reason_aws_records_on_the_failed_builds() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![
        Response::ok("{}"),
        Response::not_found(),
        Response::ok(IMAGE_CREATING),
        Response::ok(IMAGE_CREATE_FAILED),
        Response::ok(VERSION_FAILED),
        Response::ok(BUILDS_FAILED),
    ]);

    let output = run_cli(
        directory.path(),
        &["--format", "json", "push", "--timeout", "5s"],
        &fake.url(),
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("release demo@2 failed: Ready hook invocation timed out\n"),
        "{stderr}"
    );
    let requests = fake.finish();
    assert!(
        requests[5].contains("/versions/2/builds"),
        "{}",
        requests[5]
    );
}

#[test]
fn push_publishes_a_new_version_after_a_failed_build() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), FULL_CONFIG);
    let fake = FakeAws::start(vec![
        Response::ok("{}"),
        Response::not_found(),
        Response::ok(IMAGE_CREATING),
        Response::ok(IMAGE_CREATE_FAILED),
        Response::ok(VERSION_FAILED),
        Response::ok(BUILDS_FAILED),
        Response::ok("{}"),
        Response::ok(IMAGE_CREATE_FAILED),
        Response::ok(IMAGE_UPDATING),
        Response::ok(IMAGE_UPDATED),
        Response::ok(VERSION_3_ACTIVE),
    ]);

    let output = run_cli(
        directory.path(),
        &[
            "--format",
            "json",
            "push",
            "--timeout",
            "5s",
            "--build-retries",
            "1",
        ],
        &fake.url(),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["release"], "demo@3");
    assert!(
        stderr.contains("demo@2 failed: Ready hook invocation timed out; retrying"),
        "{stderr}"
    );
    let requests = fake.finish();
    assert!(
        requests[8].starts_with("PUT "),
        "a failed create is retried as an update: {}",
        requests[8]
    );
}

#[test]
fn aws_server_errors_are_retried_beyond_the_sdk_default() {
    let directory = TempDir::new().unwrap();
    write_config(directory.path(), "");
    let fake = FakeAws::start(vec![
        Response::html_server_error(),
        Response::html_server_error(),
        Response::html_server_error(),
        Response::ok(MICROVMS_NONE),
    ]);

    let result = run_json(directory.path(), &["--format", "json", "list"], &fake.url());

    assert_eq!(result["microvms"], serde_json::json!([]));
    assert_eq!(fake.finish().len(), 4);
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
        format!("[aws]\nregion = \"us-east-1\"\n[microvm]\nname = \"demo\"\n{sections}"),
    )
    .unwrap();
}
