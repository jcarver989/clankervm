# ClankerVM

ClankerVM makes AWS Lambda MicroVM applications feel like projects rather than collections of AWS API calls:

```text
source ──push──> active image release ──run──> MicroVM
```

The workspace contains:

- `clankervm`: the host-side project CLI.
- `clankervm-server`: the lifecycle hook server installed inside an image.
- `examples/minimal`: a minimal image context containing the hook server.

## Install

```sh
CLANKERVM_VERSION='<VERSION>'
curl --proto '=https' --tlsv1.2 -LsSf \
  "https://github.com/jcarver989/clankervm/releases/download/clankervm-v${CLANKERVM_VERSION}/clankervm-installer.sh" \
  | sh
```

## Usage

Configure the project once:

```sh
clankervm init \
  --name my-runner \
  --region us-west-2 \
  --artifact-bucket "$BUCKET" \
  --build-role-arn "$BUILD_ROLE_ARN" \
  --execution-role-arn "$EXECUTION_ROLE_ARN"
```

Then release and run it:

```sh
clankervm push
clankervm run -- /usr/local/bin/my-job --job-id 42
```

`push` bundles the directory configured by `[push].context` in memory, uploads it, creates or updates the named image, and waits for the exact returned image version to become active. It stores no local deployment state. An existing ZIP can be supplied with the invocation-only `push --bundle PATH` option. See the [CLI configuration reference](crates/clankervm-cli/README.md) for schema version 1 and push/run/status defaults.

`status` performs a one-shot inspection; `status --wait NAME@VERSION` waits for an exact release to become active.

`run` launches the configured image with an explicit command and arguments. The run-hook payload is limited to 4096 bytes.

See [`crates/clankervm-cli/README.md`](crates/clankervm-cli/README.md) for project configuration, prepared source contexts, release monitoring, JSON output, and run options.

ClankerVM is under active development. The AWS Lambda MicroVM service and its API may change.

## License

MIT
