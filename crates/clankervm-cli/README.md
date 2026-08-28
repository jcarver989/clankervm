# ClankerVM CLI

`clankervm` bundles, releases, inspects, and runs AWS Lambda MicroVM applications.

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

For every setting, command-line values take precedence over TOML values, which take precedence over built-in defaults. `--bundle PATH` is intentionally invocation-only and supplies an existing ZIP instead of `[push].context`. Tags must be `key=value`; malformed and duplicate tag keys are rejected. Unknown configuration fields are rejected.

For multiple images, keep each deployment unit explicit:

```sh
clankervm --config images/agent/clankervm.toml push
clankervm --config images/worker/clankervm.toml push
```

An explicit release such as `status my-runner@42` or `run --release my-runner@42` must match the image name in the selected configuration file.

ClankerVM does not compile or prepare application assets. Prepare an image directory or ZIP with Docker, a shell script, `just`, Bazel, Nix, or another build system first.

## Push

`push` accepts either a prepared directory or a prebuilt ZIP. A directory is converted to a deterministic ZIP; an existing ZIP is validated and uploaded byte-for-byte. In both cases ClankerVM uses an immutable content-addressed S3 key, creates or updates the image through the Rust AWS SDK, and waits for the exact version returned by AWS. After activation, `keep-versions = N` deletes inactive versions beyond the newest N.

With no path, `push` uses `[push].context`. The positional path overrides it. The invocation-only `--bundle` option remains supported for compatibility and is equivalent to passing the ZIP as the positional path.

```sh
clankervm push
clankervm push path/to/prepared-directory
clankervm push path/to/image.zip
clankervm push --bundle path/to/image.zip
```

## Status

```sh
clankervm status
clankervm status my-runner@42
clankervm status --wait my-runner@42
clankervm status --wait --timeout 10m my-runner@42
```

`status --wait` uses `--timeout` over `[status].timeout`; it never uses the push timeout.

## Run

```sh
clankervm run -- /usr/local/bin/my-job --job-id 42
clankervm run --release my-runner@42 --client-token "$RUN_ID" --env LOG_LEVEL=debug -- ./job
```

Run flags mirror `[run]` keys, including `--max-duration` and `max-duration`. `run.command` provides a default executable and arguments; a command passed after `--` takes precedence. `run.environment` accepts `key=value` entries and repeatable `--env key=value` flags override the configured environment. Empty values are supported, malformed or duplicate keys are rejected, and `AWS_REGION` plus `AWS_DEFAULT_REGION` are set from `image.region`. A command is required from TOML or the CLI, and the complete payload shares AWS's 4096-byte run-hook limit.

## JSON output

Use `--format json` for stable JSON on stdout. Human progress is written to stderr.
