use aws_credential_types::Credentials;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::handler::Handler;
use axum::http::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, TRANSFER_ENCODING,
};
use axum::http::{Method, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::{Router, serve};
use clankervm_sigv4_proxy::{Route, SigV4ProxyError, router};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use reqwest::redirect::Policy;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::watch;

#[tokio::test]
async fn signs_and_forwards_requests() -> Result<(), TestError> {
    let mut proxy = TestProxyBuilder::new()
        .route("/things/", "/prod", things())
        .respond(|| async { (StatusCode::IM_A_TEAPOT, "answer") })
        .start()
        .await?;

    let (status, body) = proxy
        .call(Method::POST, "/things/a/b?c=d", r#"{"e":"f"}"#)
        .await?;

    assert_eq!((status, body.as_str()), (StatusCode::IM_A_TEAPOT, "answer"));
    let request = proxy.next_request().await?;
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri(), "/prod/a/b?c=d");
    assert_signed(&request, "execute-api", "us-west-2");
    assert_eq!(header(&request, "x-api-key"), "");
    assert_eq!(header(&request, CONTENT_TYPE.as_str()), "application/json");
    assert_eq!(request.body(), r#"{"e":"f"}"#);
    Ok(())
}

#[tokio::test]
async fn sends_the_length_of_a_large_chunked_request() -> Result<(), TestError> {
    let mut proxy = TestProxyBuilder::new()
        .route("/things/", "", things())
        .start()
        .await?;
    let chunk = "x".repeat(3 << 19);
    let chunks = stream::iter([chunk.clone(), chunk].map(Ok::<_, Infallible>));

    let response = proxy
        .client
        .post(format!("{}/things/x", proxy.url))
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let request = proxy.next_request().await?;
    assert_eq!(header(&request, CONTENT_LENGTH.as_str()), "3145728");
    assert_eq!(request.headers().get(TRANSFER_ENCODING), None);
    assert_eq!(request.body().len(), 3 << 20);
    Ok(())
}

#[tokio::test]
async fn serves_each_route_at_its_path() -> Result<(), TestError> {
    let lfs = json!({ "service": "bedrock-agentcore", "region": "us-east-1" });
    let mut proxy = TestProxyBuilder::new()
        .route("/git/", "/prod", things())
        .route("/git/lfs/", "/lfs", lfs)
        .start()
        .await?;

    let (api, agentcore) = ("execute-api", "bedrock-agentcore");
    for (path, want_path, want_service, want_region) in [
        ("/git/repo/refs", "/prod/repo/refs", api, "us-west-2"),
        ("/git", "/prod", api, "us-west-2"),
        ("/git/lfs/a?b=c", "/lfs/a?b=c", agentcore, "us-east-1"),
    ] {
        let (status, _) = proxy.call(Method::GET, path, "").await?;

        assert_eq!(status, StatusCode::OK, "for {path}");
        let request = proxy.next_request().await?;
        assert_eq!(request.uri(), want_path, "for {path}");
        assert_signed(&request, want_service, want_region);
    }
    let (status, _) = proxy.call(Method::GET, "/gitea/x", "").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn sets_a_routes_headers_over_the_callers() -> Result<(), TestError> {
    let logs = json!({
        "service": "logs",
        "region": "us-west-2",
        "headers": { "X-Aws-Log-Group": "/my/group", "X-Api-Key": "the-real-key" }
    });
    let mut proxy = TestProxyBuilder::new().route("/", "", logs).start().await?;

    let response = proxy
        .client
        .post(format!("{}/v1/logs", proxy.url))
        .header("X-Aws-Log-Group", "/another/group")
        .header("X-Api-Key", "placeholder")
        .body("events")
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let request = proxy.next_request().await?;
    assert_eq!(request.uri(), "/v1/logs");
    assert_eq!(header(&request, "x-aws-log-group"), "/my/group");
    assert_eq!(header(&request, "x-api-key"), "the-real-key");
    assert_signed(&request, "logs", "us-west-2");
    assert!(header(&request, AUTHORIZATION.as_str()).contains("x-aws-log-group"));
    Ok(())
}

#[tokio::test]
async fn streams_each_event_as_it_arrives() -> Result<(), TestError> {
    const FIRST: &str = "data: first\n\n";
    const SECOND: &str = "data: second\n\n";
    let (release, released) = watch::channel(false);
    let proxy = TestProxyBuilder::new()
        .route("/things/", "", things())
        .respond(move || {
            let mut released = released.clone();
            let second = stream::once(async move {
                let _ = released.wait_for(|released| *released).await;
                SECOND
            });
            let events = stream::iter([FIRST]).chain(second).map(Ok::<_, Infallible>);
            async {
                (
                    [(CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(events),
                )
            }
        })
        .start()
        .await?;

    let mut response = proxy
        .client
        .post(format!("{}/things/events", proxy.url))
        .body("{}")
        .send()
        .await?;

    let mut events = String::new();
    while events != FIRST {
        let chunk = response.chunk().await?.ok_or(TestError::Closed)?;
        events.push_str(&String::from_utf8_lossy(&chunk));
    }
    release.send_replace(true);
    while let Some(chunk) = response.chunk().await? {
        events.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(events, format!("{FIRST}{SECOND}"));
    Ok(())
}

#[tokio::test]
async fn returns_a_redirect_instead_of_following_it() -> Result<(), TestError> {
    let mut proxy = TestProxyBuilder::new()
        .route("/things/", "", things())
        .respond(|request: Request| async move {
            match request.uri().path() {
                "/x" => (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, "/again")]).into_response(),
                _ => StatusCode::OK.into_response(),
            }
        })
        .start()
        .await?;

    let (status, _) = proxy.call(Method::GET, "/things/x", "").await?;

    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(proxy.next_request().await?.uri(), "/x");
    Ok(())
}

#[tokio::test]
async fn answers_502_when_the_endpoint_is_unreachable() -> Result<(), TestError> {
    let proxy = TestProxyBuilder::new()
        .route("/", "", things())
        .unreachable()
        .start()
        .await?;

    let (status, body) = proxy.call(Method::GET, "/x", "").await?;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.starts_with("send the request"), "got {body}");
    Ok(())
}

#[test]
fn rejects_routes_it_cannot_sign_for() {
    let endpoint = "https://xray.us-west-2.amazonaws.com";
    for route in [
        json!({ "endpoint": endpoint, "service": "xray" }),
        json!({ "endpoint": "xray.us-west-2.amazonaws.com", "service": "xray", "region": "us-west-2" }),
        json!({ "endpoint": endpoint, "service": "xray", "region": "us-west-2", "regoin": "us-east-1" }),
        json!({ "endpoint": endpoint, "service": "xray", "region": "us-west-2", "headers": { "a b": "c" } }),
    ] {
        let parsed = serde_json::from_value::<Route>(route.clone());

        assert!(parsed.is_err(), "got {parsed:?} for {route}");
    }
}

#[test]
fn rejects_a_prefix_no_path_starts_with() -> Result<(), TestError> {
    let mut route = things();
    route["endpoint"] = "https://abc123.execute-api.us-west-2.amazonaws.com".into();
    let routes = BTreeMap::from([("things/".to_string(), serde_json::from_value(route)?)]);

    let built = router(routes, credentials());

    assert!(matches!(built, Err(SigV4ProxyError::InvalidPrefix(prefix)) if prefix == "things/"));
    Ok(())
}

fn assert_signed(request: &Request<Bytes>, service: &str, region: &str) {
    let authorization = header(request, AUTHORIZATION.as_str());
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
            && authorization.contains(&format!("/{region}/{service}/aws4_request"))
            && authorization.contains("SignedHeaders=")
            && authorization.contains("host;"),
        "authorization header = {authorization:?}, want a {region} {service} signature"
    );
    assert_eq!(header(request, "x-amz-security-token"), "token");
    assert_ne!(header(request, "x-amz-date"), "");
}

fn header<'a>(request: &'a Request<Bytes>, name: &str) -> &'a str {
    let value = request.headers().get(name).map(|value| value.to_str());
    value.and_then(Result::ok).unwrap_or_default()
}

fn credentials() -> Credentials {
    Credentials::new(
        "AKIDEXAMPLE",
        "secret",
        Some("token".to_string()),
        None,
        "test",
    )
}

fn things() -> Value {
    json!({ "service": "execute-api", "region": "us-west-2" })
}

struct TestProxyBuilder {
    routes: Vec<(String, String, Value)>,
    endpoint: Router,
    reachable: bool,
}

impl TestProxyBuilder {
    fn new() -> Self {
        Self {
            routes: Vec::new(),
            endpoint: Router::new().fallback(|| async { StatusCode::OK }),
            reachable: true,
        }
    }

    fn route(mut self, prefix: &str, path: &str, route: Value) -> Self {
        self.routes
            .push((prefix.to_string(), path.to_string(), route));
        self
    }

    fn respond<T, U>(mut self, handler: T) -> Self
    where
        T: Handler<U, ()>,
        U: 'static,
    {
        self.endpoint = Router::new().fallback(handler);
        self
    }

    fn unreachable(mut self) -> Self {
        self.reachable = false;
        self
    }

    async fn start(self) -> Result<TestProxy, TestError> {
        let endpoint_listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint_url = format!("http://{}", endpoint_listener.local_addr()?);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);

        let (recorder, requests) = unbounded_channel();
        if self.reachable {
            let endpoint = self.endpoint.layer(from_fn_with_state(recorder, record));
            tokio::spawn(serve(endpoint_listener, endpoint).into_future());
        }
        let mut routes = BTreeMap::new();
        for (prefix, path, mut route) in self.routes {
            route["endpoint"] = format!("{endpoint_url}{path}").into();
            routes.insert(prefix, serde_json::from_value(route)?);
        }
        tokio::spawn(serve(listener, router(routes, credentials())?).into_future());

        Ok(TestProxy {
            url,
            client: Client::builder().redirect(Policy::none()).build()?,
            requests,
        })
    }
}

struct TestProxy {
    url: String,
    client: Client,
    requests: UnboundedReceiver<Request<Bytes>>,
}

impl TestProxy {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: &'static str,
    ) -> Result<(StatusCode, String), TestError> {
        let response = self
            .client
            .request(method, format!("{}{path}", self.url))
            .header(AUTHORIZATION, "Bearer placeholder")
            .header("X-Api-Key", "placeholder")
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        Ok((response.status(), response.text().await?))
    }

    async fn next_request(&mut self) -> Result<Request<Bytes>, TestError> {
        self.requests.recv().await.ok_or(TestError::Closed)
    }
}

async fn record(
    State(recorder): State<UnboundedSender<Request<Bytes>>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, usize::MAX).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let _ = recorder.send(Request::from_parts(parts.clone(), body.clone()));
    next.run(Request::from_parts(parts, Body::from(body))).await
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Proxy(#[from] SigV4ProxyError),
    #[error("the endpoint or the response closed early")]
    Closed,
}
