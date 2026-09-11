# ClankerVM Server

ClankerVM makes it a cinch to run a single, supervised command in a Lambda MicroVM, pass it some args + environment variables and let 'er rip -- e.g. a coding agent via `claude -p ...` equipped with a scoped `GITHUB_TOKEN`.

It's a small [Axum](https://github.com/tokio-rs/axum) HTTP server that runs in your AWS Lambda MicroVM. It handles [lifecycle hooks](https://docs.aws.amazon.com/lambda/latest/dg/microvms-launching.html#microvms-launching-lifecycle-hooks) for you. Install it in your image. Then launch your microVM with a `--run-hook-payload` that matches this JSON shape:

```json
{
  "command": "/path/to/executable",
  "args": ["arg1", "arg2"],
  "environment": { "NAME": "value" }
}
```

## Usage

### 1. Install the server and build your MicroVM image

Use the canonical [`examples/minimal/Dockerfile`](../../examples/minimal/Dockerfile) as a starting point. It selects the release artifact for the image architecture, verifies its SHA-256 checksum, and installs `clankervm-server` without requiring a Rust toolchain. Change its `CLANKERVM_SERVER_VERSION` argument to pin a different server release.

See [AWS docs](https://docs.aws.amazon.com/lambda/latest/dg/microvms-getting-started.html) for more details.

### 2. Run the MicroVM

```sh
RUN_HOOK_PAYLOAD='{
  "command": "/usr/local/bin/my-agent",
  "args": ["--task", "fix the tests"],
  "environment": {
    "WORKSPACE": "/workspace",
    "LOG_LEVEL": "info"
  }
}'

aws lambda-microvms run-microvm \
  --image-identifier "$IMAGE_ARN" \
  --execution-role-arn "$EXECUTION_ROLE_ARN" \
  --run-hook-payload "$RUN_HOOK_PAYLOAD"
```

## Pre-snapshot initialization

The CLI's `[microvm.image.hooks.ready]` table supplies an optional initialization
command via the `CLANKERVM_READY_HOOK_PAYLOAD` image environment variable. It contains
the same JSON shape as the run payload above. For manual use, set that variable when
creating/updating the image, or pass `--ready-hook-payload` to the server.

Initialization starts once when the server starts. `/ready` returns HTTP 503
immediately while it runs, then HTTP 200 after successful completion. A failed
initialization stops the server with a nonzero exit status. `/run` is unavailable
until initialization succeeds, and the later run command is still independent.
Shutdown signals and `/terminate` cancel initialization and wait for its process
group. The command inherits the server's user and working directory.

AWS snapshots the completed state, so restored MicroVMs do not repeat initialization.
Do not place secret values in image configuration or leave credentials or per-run
identities in the snapshotted state. With no ready payload, `/ready` retains its
immediate-success behavior. See the [AWS image lifecycle](https://docs.aws.amazon.com/lambda/latest/dg/microvms-images.html).

## Recipes

### Secrets

For sensitive secrets, rather than putting them directly into the `environment` section of your `--run-hook-payload`, you can: 

1. Put a reference to their name (e.g. a `SecretsManager` secret name or an SSM `SecureString` path) in the `args` or `environment` fields in the payload
2. Bake a `setup.sh` script into your microVM image that reads the secret names from the `args` or `environment` and uses the AWS CLI or SDK to fetch the secret values.
