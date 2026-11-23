use crate::error::{ForwardError, SigV4ProxyError};
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4::SigningParams;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, EXPECT, FORWARDED, HOST, HeaderName,
    PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, ToStrError, UPGRADE,
};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use reqwest::Client;
use reqwest::redirect::Policy;
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::SystemTime;
use url::Url;

const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub endpoint: Url,
    pub service: String,
    pub region: String,
    #[serde(default, deserialize_with = "header_map")]
    pub headers: HeaderMap,
}

struct Proxy {
    routes: BTreeMap<String, Route>,
    credentials: SharedCredentialsProvider,
    client: Client,
}

pub fn router(
    routes: BTreeMap<String, Route>,
    credentials: impl ProvideCredentials + 'static,
) -> Result<Router, SigV4ProxyError> {
    if let Some(prefix) = routes.keys().find(|prefix| !prefix.starts_with('/')) {
        return Err(SigV4ProxyError::InvalidPrefix(prefix.clone()));
    }
    let proxy = Proxy {
        routes,
        credentials: SharedCredentialsProvider::new(credentials),
        client: Client::builder().redirect(Policy::none()).build()?,
    };
    Ok(Router::new()
        .fallback(forward)
        .layer(DefaultBodyLimit::disable())
        .with_state(Arc::new(proxy)))
}

async fn forward(
    State(proxy): State<Arc<Proxy>>,
    method: Method,
    uri: Uri,
    mut headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ForwardError> {
    let route = proxy.routes.iter().filter_map(|(prefix, route)| {
        let rest = uri.path().strip_prefix(prefix.trim_end_matches('/'))?;
        (rest.is_empty() || rest.starts_with('/')).then_some((prefix.len(), route, rest))
    });
    let Some((_, route, rest)) = route.max_by_key(|(length, ..)| *length) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let endpoint = route.endpoint.as_str().trim_end_matches('/');
    let query = uri.query().map_or_default(|query| format!("?{query}"));

    remove_hop_by_hop(&mut headers);
    for name in [
        HOST,
        CONTENT_LENGTH,
        EXPECT,
        AUTHORIZATION,
        X_API_KEY,
        FORWARDED,
        X_FORWARDED_FOR,
        X_FORWARDED_HOST,
        X_FORWARDED_PROTO,
    ] {
        headers.remove(name);
    }
    headers.extend(route.headers.clone());
    let mut request = Request::builder()
        .method(method)
        .uri(format!("{endpoint}{rest}{query}"))
        .body(body)?;
    *request.headers_mut() = headers;

    let identity = proxy.credentials.provide_credentials().await?.into();
    let params = SigningParams::builder()
        .identity(&identity)
        .region(&route.region)
        .name(&route.service)
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()?
        .into();
    let signable_headers = request
        .headers()
        .iter()
        .map(|(name, value)| Ok((name.as_str(), value.to_str()?)))
        .collect::<Result<Vec<_>, ToStrError>>()?;
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.uri().to_string(),
        signable_headers.into_iter(),
        SignableBody::Bytes(request.body()),
    )?;
    let (instructions, _) = sign(signable, &params)?.into_parts();
    instructions.apply_to_request_http1x(&mut request);

    let response = proxy.client.execute(request.try_into()?).await?;
    let status = response.status();
    let mut headers = response.headers().clone();
    remove_hop_by_hop(&mut headers);
    Ok((status, headers, Body::new(reqwest::Body::from(response))).into_response())
}

fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::try_from(name.trim()).ok())
        .collect();
    let fixed = [
        CONNECTION,
        KEEP_ALIVE,
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ];
    for name in named.into_iter().chain(fixed) {
        headers.remove(name);
    }
}

fn header_map<'de, T: Deserializer<'de>>(deserializer: T) -> Result<HeaderMap, T::Error> {
    let headers = HashMap::<String, String>::deserialize(deserializer)?;
    HeaderMap::try_from(&headers).map_err(T::Error::custom)
}
