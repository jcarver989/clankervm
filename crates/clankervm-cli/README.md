# clankervm CLI

`clankervm` deploys and runs AWS Lambda MicroVM images.

## Requirements

- AWS credentials available through the standard AWS SDK credential chain.
- A prepared image directory or ZIP file.
- An S3 artifact bucket and IAM roles for building and running MicroVMs.

## Create a project

```sh
clankervm init \
  --name my-runner \
  --region us-west-2 \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  --execution-role-arn "$EXECUTION_ROLE_ARN"
```

This creates `clankervm.toml`. Pass `--force` to replace an existing file.

## Configuration

A project file with all supported settings:

```toml
[aws]
region = "us-west-2"
profile = "my-aws-profile"

[microvm]
name = "my-runner"

[microvm.image]
base-image = "al2023-1"
minimum-memory-mib = 512
os-capabilities = ["ALL"]
iam-role = "arn:aws:iam::123456789012:role/clankervm-build"
tags = ["team=platform"]

[microvm.image.artifact]
source = "."
s3-bucket = "my-artifact-bucket"
s3-prefix = "clankervm"

[microvm.image.network]
egress = "INTERNET_EGRESS"

[microvm.image.hooks]
port = 9000
ready-timeout = "5m"
run-timeout = "1m"
terminate-timeout = "30s"

[microvm.image.versions]
max = 10
wait-timeout = "1h"

[microvm.run]
iam-role = "arn:aws:iam::123456789012:role/clankervm-execution"
command = ["/usr/local/bin/my-job", "--job-id", "42"]
environment = ["LOG_LEVEL=info"]
max-duration = 3600

[microvm.run.network]
ingress = "NO_INGRESS"
egress = "INTERNET_EGRESS"

[microvm.run.logs]
group = "/my-runner/microvms"
stream = "..."
since = "30m"
limit = 1000
timeout = "1m"
```

Only `aws.region` and `microvm.name` are required to load a project. Image and
run tables are optional; each command checks its required settings (for example,
push needs an artifact bucket and image IAM role). Unknown fields are rejected.

Command-line settings override TOML settings; existing flag names are unchanged.
For example, `--artifact-bucket` overrides `microvm.image.artifact.s3-bucket`,
`--build-role-arn` overrides `microvm.image.iam-role`, and `--execution-role-arn`
overrides `microvm.run.iam-role`. Lists supplied on the command line replace
configured lists rather than appending to them. `--region` overrides `aws.region`.

Artifact sources are resolved relative to the project file, defaulting to `.`.
Hook timeouts use duration strings and must fit in a whole number of signed
32-bit seconds; the corresponding CLI flags still accept integer seconds.
`microvm.image.versions.max` controls pruning after a successful push; omitting
it disables pruning. `wait-timeout` applies to both push and `status --wait`
(default `1h`), and each command's `--timeout` overrides it.

`microvm.run.logs.group` sets both the launch log destination and the group read
by `logs`. If omitted, logs uses `/aws/lambda-microvms/<name>`. `stream` defaults
to the MicroVM ID and only affects log reads, as do `since`, `limit`, and `timeout`.
`max-duration` is in seconds; log `since` and `timeout` use duration strings.

Use one configuration file per image:

```sh
clankervm --config images/agent.toml push
clankervm --config images/worker.toml run -- ./worker
```

## Push an image

Use `microvm.image.artifact.source`, or pass a directory or ZIP explicitly:

```sh
clankervm push
clankervm push path/to/prepared-directory
clankervm push path/to/image.zip
```

Override settings for one release:

```sh
clankervm push \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  --base-image al2023-1 \
  --tag team=platform \
  --tag imageName=my-runner
```

## Check release status

```sh
clankervm status
clankervm status my-runner@42
clankervm status --wait my-runner@42
clankervm status --wait --timeout 10m my-runner@42
```

## List MicroVMs

Terminated MicroVMs are hidden unless `--all` or an explicit `--state` is used.

```sh
clankervm list
clankervm list --image my-runner
clankervm list --image my-runner --image-version 42
clankervm list --state RUNNING --state PENDING
clankervm list --state TERMINATED
clankervm --format json list --all
```

Available states: `PENDING`, `RUNNING`, `SUSPENDED`, `SUSPENDING`,
`TERMINATING`, and `TERMINATED`.

## Run a command

```sh
clankervm run -- /usr/local/bin/my-job --job-id 42
clankervm run --release my-runner@42 -- ./job
clankervm run --env LOG_LEVEL=debug --env DRY_RUN=false -- ./job
```

Use `microvm.run.command` when a command should be the default. A command after `--`
overrides it. Environment entries use `KEY=VALUE`.

Useful run options:

```sh
clankervm run \
  --execution-role-arn "$EXECUTION_ROLE_ARN" \
  --max-duration 3600 \
  --ingress NO_INGRESS \
  --egress INTERNET_EGRESS \
  -- ./job
```

## Attach a shell

Run `shell` from an interactive terminal:

```sh
# Launch, attach, and terminate a MicroVM on exit
clankervm shell

# Attach to an existing SHELL_INGRESS MicroVM
clankervm shell microvm-0099

# Keep a launched MicroVM after detaching
clankervm shell --keep

# Choose the command launched in the MicroVM
clankervm shell -- htop
```

Press `Ctrl-]` to detach. A launched MicroVM uses the `SHELL_INGRESS`
connector and is terminated when the session ends unless `--keep` is set.
`shell` is interactive-only and cannot be combined with `--format json`.

## Read logs

```sh
clankervm logs microvm-0099
clankervm logs microvm-0099 --follow
clankervm logs microvm-0099 --since 30m --limit 500
clankervm logs microvm-0099 --raw
clankervm logs microvm-0099 \
  --log-group /my-runner/microvms \
  --log-stream custom
```

`--follow` prints until Ctrl-C and cannot be combined with `--format json`.

## JSON output

Use the global option for machine-readable output:

```sh
clankervm --format json status
clankervm --format json list
clankervm --format json logs microvm-0099
```

Progress messages go to stderr. Run `clankervm COMMAND --help` for the complete
option list.
