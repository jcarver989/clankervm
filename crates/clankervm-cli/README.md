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

AWS credentials use the standard AWS SDK credential chain and are never stored in the project file. If any required deployment values are omitted from `init`, it writes their keys with empty-string placeholders so the generated file remains an explicit checklist.

## Configuration (schema version 1)

`clankervm.toml` keeps the AWS region in `[app]`. A single-image project can use the legacy flat `[push]` and `[run]` sections. Multi-image projects define each image under one `[image.<name>]` table; status waiting has its own independent timeout under `[status]`.

```toml
schema-version = 1

[app]
name = "my-runner"
region = "us-west-2"

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

Projects may define multiple named image profiles. Root `[push]` and `[run]` values are shared defaults, and each profile overrides only the values specified under its own table; `[app].region` and `[status]` remain global:

```toml
[image.agent]
context = "agent"
artifact-bucket = "my-microvm-artifacts"
build-role-arn = "arn:aws:iam::123456789012:role/Build"
execution-role-arn = "arn:aws:iam::123456789012:role/RunAgent"
log-group = "/agent/microvms"
run-egress = "INTERNET_EGRESS"

[image.worker]
context = "worker"
artifact-bucket = "my-microvm-artifacts"
build-role-arn = "arn:aws:iam::123456789012:role/Build"
execution-role-arn = "arn:aws:iam::123456789012:role/RunWorker"
log-group = "/worker/microvms"
```

When profiles are present, `push`, `status`, and `run` require `--image NAME` for the latest image. An explicit release such as `status agent@42` or `run --release agent@42` selects the profile automatically; passing a conflicting `--image` is rejected. The selected profile name is used for the image ARN, release, build logs, and S3 prefix. Precedence is CLI flags, then the selected profile, then root `[push]` and `[run]`, then built-in defaults. With no profiles, the legacy flat `[push]` and `[run]` configuration remains unchanged.

ClankerVM does not compile or prepare application assets. Prepare the directory configured by `push.context` with Docker, a shell script, `just`, Bazel, Nix, or another build system first.

## Push

`push` creates a deterministic ZIP from `push.context`, uploads it under an immutable content-addressed S3 key, creates or updates the image, and waits for the exact version returned by AWS. After activation, `keep-versions = N` deletes inactive versions beyond the newest N.

```sh
clankervm push
clankervm push --image agent --bundle path/to/image.zip
```

## Status

```sh
clankervm status
clankervm status --image agent
clankervm status my-runner@42
clankervm status --wait my-runner@42
clankervm status --wait --timeout 10m my-runner@42
```

`status --wait` uses `--timeout` over `[status].timeout`; it never uses the push timeout.

## Run

```sh
clankervm run -- /usr/local/bin/my-job --job-id 42
clankervm run --image agent --release agent@42 --client-token "$RUN_ID" -- ./job
```

Run flags mirror `[run]` keys, including `--max-duration` and `max-duration`. The explicit command and arguments share AWS's 4096-byte run-hook payload limit.

## JSON output

Use `--format json` for stable JSON on stdout. Human progress is written to stderr.
