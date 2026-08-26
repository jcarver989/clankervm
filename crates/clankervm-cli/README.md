# ClankerVM

`clankervm` is a Docker-shaped CLI for building AWS Lambda MicroVM images and running arbitrary commands in them.

## Build

Package a build context, upload it to S3, create or update the named image, and wait for the remote build to finish:

```sh
clankervm build -t my-runner \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  ./image
```

The context is packaged recursively without assuming a particular repository, language, or application. Artifacts are named by their SHA-256 digest. Pass `--no-wait` to return after AWS accepts the create or update request.

## Run

Run a command using Docker-style `IMAGE COMMAND ARG...` syntax:

```sh
clankervm run \
  --execution-role-arn "$EXECUTION_ROLE_ARN" \
  --env-file runtime.env \
  my-runner \
  /usr/local/bin/my-job --job-id 42
```

Arguments beginning with `-` after the image are passed to the command. Use `-e NAME=value`, `-e NAME`, or `--env-file PATH` to provide environment variables. `-e NAME` requires that variable in the host environment, and explicit `-e` entries override values loaded from `--env-file`. The selected region is authoritative for `AWS_REGION` and `AWS_DEFAULT_REGION` inside the job.

A local script can be embedded in the AWS run-hook payload:

```sh
clankervm run \
  --execution-role-arn "$EXECUTION_ROLE_ARN" \
  --script ./job.sh \
  my-runner arg1 arg2
```

The complete command, arguments, script, and environment must fit within AWS's 4096-byte run-hook payload limit. Payload-size errors list environment keys but never values.

## Configuration

`AWS_REGION` is required. Image names also require `AWS_ACCOUNT_ID`; a complete image ARN does not. Infrastructure flags support corresponding `CLANKERVM_*` environment variables shown by `clankervm build --help` and `clankervm run --help`.

The image must run `clankervm-server` on the same port configured during `clankervm build`.
