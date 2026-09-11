# ClankerVM

Deploy and run AWS Lambda MicroVMs from a project directory.

## Install

```sh
CLANKERVM_VERSION='<VERSION>'
curl --proto '=https' --tlsv1.2 -LsSf \
  "https://github.com/jcarver989/clankervm/releases/download/clankervm-v${CLANKERVM_VERSION}/clankervm-installer.sh" \
  | sh
```

## Quick start

1. Prepare an image directory or ZIP.
2. Create a project file:

```sh
clankervm init \
  --name my-runner \
  --region us-west-2 \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  --execution-role-arn "$EXECUTION_ROLE_ARN"
```

3. Release and run the image:

```sh
clankervm push
clankervm run -- /usr/local/bin/my-job --job-id 42
```

AWS credentials come from the standard AWS SDK credential chain. Use
`image.profile` in `clankervm.toml` to select a named profile.

## Commands

```sh
# Create or replace a project file
clankervm init --name NAME --region REGION [--force]

# Release the configured context, directory, or ZIP
clankervm push
clankervm push path/to/image
clankervm push path/to/image.zip

# Inspect a release
clankervm status
clankervm status NAME@VERSION
clankervm status --wait NAME@VERSION

# List MicroVMs
clankervm list
clankervm list --image NAME --state RUNNING
clankervm --format json list --all

# Run a command
clankervm run -- /path/to/program ARG...
clankervm run --release NAME@VERSION -- ./job

# Attach an interactive shell
clankervm shell
clankervm shell MICROVM_ID
clankervm shell --keep
# Press Ctrl-] to detach.

# Read logs
clankervm logs MICROVM_ID
clankervm logs MICROVM_ID --follow
clankervm logs MICROVM_ID --since 30m --limit 500
```

## Project files

Each `clankervm.toml` describes one image. Use one file per image and select it
with `--config`:

```sh
clankervm --config images/worker.toml push
```

See the [CLI usage reference](crates/clankervm-cli/README.md) for configuration
fields, command options, and JSON output.

## Global options

```sh
clankervm --config PATH COMMAND
clankervm --region REGION COMMAND
clankervm --format human COMMAND
clankervm --format json COMMAND
```

Run `clankervm --help` or `clankervm COMMAND --help` for all options.
