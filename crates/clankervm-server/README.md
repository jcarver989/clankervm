# ClankerVM Server

A small HTTP server that runs in an AWS Lambda MicroVM and handles its [lifecycle hooks](https://docs.aws.amazon.com/lambda/latest/dg/microvms-launching.html#microvms-launching-lifecycle-hooks) for you. 

It makes it a cinch to run an arbitrary command with environment variables inside a Lambda MicroVM (e.g. a coding agent a scoped, ephemeral `GITHUB_TOKEN`). Install the server in your MicroVM image and launch your MicroVM with a `--run-hook-payload` that matches this JSON shape:

```json
{
  "command": "/path/to/executable",
  "args": ["arg1", "arg2"],
  "environment": { "NAME": "value" }
}
```

The ClankerVM server receives this payload via the `/run` lifecycle hook, then runs the specified `command` in a separate process with the specified `args` and `environment` variables and supervises it for you.

## Usage

### 1. Install the server and build your MicroVM image

In your MicroVM image's `Dockerfile`:

```dockerfile
# Install the server 
FROM rust:1.97-bookworm AS build
RUN cargo install --locked clankervm-server

# Run the server
FROM debian:bookworm-slim
COPY --from=build /usr/local/cargo/bin/clankervm-server /usr/local/bin/
CMD ["clankervm-server"]
```

See [AWS docs](https://docs.aws.amazon.com/lambda/latest/dg/microvms-getting-started.html) for more details.

### 2. Run the MicroVM

```sh
RUN_HOOK_PAYLOAD='{"command":"/usr/local/bin/my-agent","args":["--task","fix the tests"],"environment":{"WORKSPACE":"/workspace","LOG_LEVEL":"info"}}'

aws lambda-microvms run-microvm \
  --image-identifier "$IMAGE_ARN" \
  --execution-role-arn "$EXECUTION_ROLE_ARN" \
  --run-hook-payload "$RUN_HOOK_PAYLOAD"
```
