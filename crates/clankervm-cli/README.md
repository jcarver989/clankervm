# ClankerVM CLI

`clankervm` bundles, releases, inspects, runs, and reads the logs of AWS Lambda MicroVM applications.

## Quick start

```sh
clankervm init --name my-runner --region us-west-2 \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  --execution-role-arn "$EXECUTION_ROLE_ARN"
clankervm push
clankervm run -- echo 'hello from a MicroVM'
```

AWS credentials use the standard AWS SDK credential chain and are never stored in the project file. Set `image.profile` to select a named AWS profile without storing its credentials. If any required deployment values are omitted from `init`, it writes their keys with empty-string placeholders so the generated file remains an explicit checklist.

## Configuration (schema version 1)

Each configuration file describes exactly one image. Repositories with multiple images use one file per image and select it with the global `--config PATH` option. ClankerVM intentionally has no profile inheritance or configuration includes.

```toml
schema-version = 1

[image]
name = "my-runner"
region = "us-west-2"
profile = "Production-PowerUser"

[push]
context = "image"
artifact-bucket = "my-microvm-artifacts"
build-role-arn = "arn:aws:iam::123456789012:role/MicroVmBuildRole"
base-image = "al2023-1"
minimum-memory-mib = 4096
capabilities = ["ALL"]
egress = "INTERNET_EGRESS"
keep-versions = 3
tags = ["imageName=my-runner", "team=platform"]
port = 9000
ready-timeout-seconds = 300
run-timeout-seconds = 60
terminate-timeout-seconds = 30
timeout = "1h"

[status]
timeout = "1h"

[logs]
log-group = "/my-runner/microvms"
since = "30m"
limit = 500
timeout = "1m"

[run]
command = ["/usr/local/bin/my-job", "--job-id", "42"]
environment = ["LOG_LEVEL=info", "DRY_RUN=false"]
execution-role-arn = "arn:aws:iam::123456789012:role/MicroVmExecutionRole"
log-group = "/my-runner/microvms"
max-duration = 3600
ingress = "NO_INGRESS"
egress = "INTERNET_EGRESS"
```

All push settings have corresponding `push` flags. For example:

```sh
clankervm push --context image --base-image al2023-1 \
  --artifact-bucket "$BUCKET" --build-role-arn "$BUILD_ROLE_ARN" \
  --tag imageName=my-runner --tag team=platform
```

For every setting, command-line values take precedence over TOML values, which take precedence over built-in defaults. The positional `push PATH` is intentionally invocation-only and supplies a directory or existing ZIP instead of `[push].context`. Tags must be `key=value`; malformed and duplicate tag keys are rejected. Unknown configuration fields are rejected. The `[logs]` table has matching `--log-group`, `--log-stream`, `--since`, `--limit` and `--timeout` flags.

For multiple images, keep each deployment unit explicit:

```sh
clankervm --config images/agent/clankervm.toml push
clankervm --config images/worker/clankervm.toml push
```

An explicit release such as `status my-runner@42` or `run --release my-runner@42` must match the image name in the selected configuration file.

ClankerVM does not compile or prepare application assets. Prepare an image directory or ZIP with Docker, a shell script, `just`, Bazel, Nix, or another build system first.

## Push

`push` accepts either a prepared directory or a prebuilt ZIP. A directory is converted to a deterministic ZIP; an existing ZIP is validated and uploaded byte-for-byte. In both cases ClankerVM uses an immutable content-addressed S3 key, creates or updates the image through the Rust AWS SDK, and waits for the exact version returned by AWS. After activation, `keep-versions = N` deletes inactive versions beyond the newest N.

With no path, `push` uses `[push].context`. The positional path overrides it.

```sh
clankervm push
clankervm push path/to/prepared-directory
clankervm push path/to/image.zip
```

## Status

```sh
clankervm status
clankervm status my-runner@42
clankervm status --wait my-runner@42
clankervm status --wait --timeout 10m my-runner@42
```

`status --wait` uses `--timeout` over `[status].timeout`; it never uses the push timeout.

## List

`list` reports the MicroVMs visible in the configured account and region so you can find an id before inspecting or terminating it. It never launches or terminates anything, and it needs neither the push nor the run role: `lambda:ListMicrovms` on the selected credentials is enough.

```sh
clankervm list
clankervm list --image my-runner --image-version 42
clankervm list --state RUNNING --state PENDING
clankervm --format json list --all
```

Terminated MicroVMs are hidden by default. `--all` includes them, while repeatable `--state` filters replace that default entirely, so `--state TERMINATED` works on its own; the two options are mutually exclusive. States are matched case-insensitively against AWS's values (`PENDING`, `RUNNING`, `SUSPENDED`, `SUSPENDING`, `TERMINATING`, `TERMINATED`); unsupported filters are rejected. `--image` accepts a name or an ARN, and `--image-version` requires it. An explicit ARN must belong to the configured region.

Every page AWS returns is read within `--timeout` (default `1m`), including empty pages that still carry a next token. Results are deduplicated by id and sorted newest first, and a failure on any later page fails the command instead of printing a partial listing. There are no per-MicroVM detail calls, so a listing is a best-effort snapshot rather than an ownership inventory.

Human output names the region and the credential scope:

```text
Region:  us-west-2
Profile: Production-PowerUser

VM ID         STATE    IMAGE                                                       VERSION  STARTED
microvm-0099  RUNNING  arn:aws:lambda:us-west-2:123456789012:microvm-image:my-runner 42       2026-08-25T00:00:00Z
```

JSON output is `{"microvms":[{"microvmId":...,"state":...,"imageArn":...,"imageVersion":...,"startedAt":...}]}` with RFC3339 UTC timestamps and an empty array when nothing matches.

## Run

```sh
clankervm run -- /usr/local/bin/my-job --job-id 42
clankervm run --release my-runner@42 --client-token "$RUN_ID" --env LOG_LEVEL=debug -- ./job
```

Run flags mirror `[run]` keys, including `--max-duration` and `max-duration`. `run.command` provides a default executable and arguments; a command passed after `--` takes precedence. `run.environment` accepts `key=value` entries and repeatable `--env key=value` flags override the configured environment. Empty values are supported, malformed or duplicate keys are rejected, and `AWS_REGION` plus `AWS_DEFAULT_REGION` are set from `image.region`. A command is required from TOML or the CLI, and the complete payload shares AWS's 4096-byte run-hook limit.

## Logs

`logs` reads the CloudWatch Logs of one MicroVM: the application's standard output and error, plus the hook server's own messages. It needs only the MicroVM id that `run` or `list` reports, and makes no MicroVM API call.

```sh
clankervm logs microvm-0099
clankervm logs microvm-0099 --follow
clankervm logs microvm-0099 --since 30m --limit 500
clankervm logs microvm-0099 --log-group /my-runner/microvms --log-stream custom
clankervm --format json logs microvm-0099
```

AWS streams a MicroVM's logs to the group `run` configures, or, without a configured group, to `/aws/lambda-microvms/<image-name>`, which also holds the build logs. The stream is named after the MicroVM id unless the launch named another one. `logs` resolves the group in this order: `--log-group`, `[logs].log-group`, `[run].log-group` (the group this project's `run` writes to), and `/aws/lambda-microvms/<image-name>`. It resolves the stream from `--log-stream`, `[logs].log-stream`, and the MicroVM id.

One read takes at most `--timeout` (default `1m`) and reports at most `--limit` events (default `1000`, maximum `10000`). Without `--since`, the newest events in the stream are reported. With `--since 30m`, every event of the last half hour is read, page by page, up to the limit. Events are always reported oldest first.

`--follow` reads from the oldest event in the stream and keeps printing new events every second until Ctrl-C. It reads a stream a running MicroVM may still be appending to, so it is human-only: `--follow` with `--format json` is rejected. `--raw` prints event messages without their timestamps, while following or not.

Human output prints one event per line, and names the stream when there is nothing to print:

```text
2026-08-25T00:00:01Z  Hook server listening
2026-08-25T00:00:01Z  hello from a MicroVM
```

JSON output is `{"logGroup":...,"logStream":...,"events":[{"timestamp":...,"message":...}]}` with RFC3339 UTC timestamps and an empty array when nothing matches.

Reading logs needs `logs:GetLogEvents` and `logs:DescribeLogStreams` on the selected credentials. Producing them needs the MicroVM's execution role to allow `logs:CreateLogGroup`, `logs:CreateLogStream` and `logs:PutLogEvents`; without those permissions a MicroVM writes no runtime logs to read. A stream that does not exist is reported together with the streams the group does have, so a wrong `--log-stream` is obvious.

## JSON output

Use `--format json` for stable JSON on stdout. Human progress is written to stderr. `logs --follow` streams events to stdout for as long as it runs, one event per line, which is why it is human-only.
