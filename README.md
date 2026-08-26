# ClankerVM

ClankerVM builds AWS Lambda `MicroVM` images and runs arbitrary commands in them using a Docker-shaped CLI.

```sh
export AWS_REGION=us-east-1
export AWS_ACCOUNT_ID=123456789012

clankervm build -t my-runner \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  ./image

clankervm run \
  --execution-role-arn "$EXECUTION_ROLE_ARN" \
  --env-file runtime.env \
  my-runner \
  /usr/local/bin/my-job --job-id 42
```

`clankervm build [OPTIONS] CONTEXT` recursively packages the supplied context, uploads a content-addressed artifact to S3, creates or updates the named image, and waits for the remote build to finish. Pass `--no-wait` for asynchronous automation. The context decides what software is available inside the image; ClankerVM does not assume a repository, language, or agent.

`clankervm run [OPTIONS] IMAGE [COMMAND] [ARG...]` launches the image with a generic command, arguments, and environment. `--script PATH` embeds a local script in the run-hook payload. Complete image and network connector ARNs are accepted in place of their short names.

The workspace contains:

- `clankervm`: the host-side build and run CLI.
- `clankervm-server`: the lifecycle hook server installed inside a Lambda `MicroVM` image.
- `examples/minimal`: a minimal image context containing only the hook server.

The command, arguments, embedded script, and environment share AWS's 4096-byte run-hook payload limit. ClankerVM reports environment key names, but not values, when the payload is too large.

ClankerVM is under active development. The AWS Lambda `MicroVM` service and its API may change.

## License

MIT
