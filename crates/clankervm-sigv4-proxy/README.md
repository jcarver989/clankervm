# ClankerVM SigV4 Proxy

`clankervm-sigv4-proxy` is an HTTP proxy that signs requests with [SigV4](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv.html). Run it in your AWS Lambda MicroVM to let tools that can't sign requests, such as agent harnesses, call AWS endpoints like API Gateway, Bedrock AgentCore and CloudWatch. The proxy signs each request with the MicroVM's execution role, so the tools need no credentials.

## Usage

### 1. Install the proxy in your MicroVM image

Each release has an archive per Linux architecture, next to its SHA-256 checksum:

```sh
VERSION='<VERSION>'
target=aarch64-unknown-linux-gnu # or x86_64-unknown-linux-gnu
archive="clankervm-sigv4-proxy-${target}.tar.xz"
release_url="https://github.com/jcarver989/clankervm/releases/download/clankervm-sigv4-proxy-v${VERSION}"
curl --fail --location --silent --show-error --remote-name "$release_url/$archive"
curl --fail --location --silent --show-error --remote-name "$release_url/$archive.sha256"
sha256sum --check "$archive.sha256"
tar --extract --file "$archive" --xz
install --mode 0755 "${archive%.tar.xz}/clankervm-sigv4-proxy" /usr/local/bin/clankervm-sigv4-proxy
```

### 2. Start it with the endpoints to serve

```sh
SIGV4_PROXY_ROUTES='{
  "/": {
    "endpoint": "https://my-gateway.gateway.bedrock-agentcore.us-west-2.amazonaws.com",
    "service": "bedrock-agentcore",
    "region": "us-west-2"
  },
  "/api/": {
    "endpoint": "https://abc123.execute-api.us-west-2.amazonaws.com/prod",
    "service": "execute-api",
    "region": "us-west-2"
  },
  "/otel-logs/": {
    "endpoint": "https://logs.us-west-2.amazonaws.com",
    "service": "logs",
    "region": "us-west-2",
    "headers": { "X-Aws-Log-Group": "/my/agent-logs", "X-Aws-Log-Stream": "agent" }
  }
}' clankervm-sigv4-proxy
```

The proxy then listens on `127.0.0.1:8790`:

```sh
curl http://127.0.0.1:8790/api/things?limit=3   # GET https://abc123.execute-api.us-west-2.amazonaws.com/prod/things?limit=3
```

| Option | Environment variable | Default | |
| --- | --- | --- | --- |
| `--routes` | `SIGV4_PROXY_ROUTES` | required | JSON object of the routes to serve, by path prefix |
| `--port` | `SIGV4_PROXY_PORT` | `127.0.0.1:8790` | Address to listen on |
| | `RUST_LOG` | `info` | [Log filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) |

The proxy signs for anyone who can reach it, so keep it on loopback.

## Routes

Each key of the routes is the path prefix the proxy serves an endpoint at, starting with a slash. A request for `<prefix>/a/b?c=d` goes to `<endpoint>/a/b?c=d`.

| Field | |
| --- | --- |
| `endpoint` | The endpoint's base URL. |
| `service` | The AWS service to sign for: `execute-api`, `bedrock-agentcore`, `xray`, `logs`. |
| `region` | The AWS region to sign for. |
| `headers` | Optional headers to set on each request to the endpoint, replacing the caller's. |

A request goes to the route with the longest prefix its path starts with, so a route at `/` takes whatever the others don't. Without one, other paths get a 404.

The proxy sends a signed request to the endpoint you name and nowhere else: it never follows a redirect, and the caller gets the endpoint's redirect instead.

## What the endpoint gets

- The caller's method, path, query, headers and body, signed with the credentials from the [standard AWS SDK credential chain](https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html), which in a MicroVM is its execution role. The proxy asks for them on each request, so it picks up rotated credentials.
- Not the caller's `Authorization` or `X-Api-Key`. Harnesses insist on sending a key, so give them a placeholder. To send an endpoint a real `X-Api-Key`, set it in the route's `headers`.
- The whole body at once, with its `Content-Length`. A signature covers the body, so the proxy holds each request's body in memory before it sends it.

Responses stream back as they arrive, so a model's server-sent events reach the caller one by one. The proxy answers 400 for a request it can't sign, and 502 with the reason when it can't get credentials or reach the endpoint.
