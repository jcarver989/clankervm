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

## Recipes

### Secrets

For sensitive secrets, rather than putting them directly into the `environment` section of your `--run-hook-payload`, you can: 

1. Put a reference to their name (e.g. a `SecretsManager` secret name or an SSM `SecureString` path) in the `args` or `environment` fields in the payload
2. Bake a `setup.sh` script into your microVM image that reads the secret names from the `args` or `environment` and uses the AWS CLI or SDK to fetch the secret values.
