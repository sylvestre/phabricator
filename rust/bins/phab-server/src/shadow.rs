//! Shadow mode: PHP answers, Rust compares.
//!
//! For the methods in `PHAB_SHADOW_METHODS`, the service forwards the request
//! to PHP (`PHAB_PHP_URL`, with `X-Phab-Skip-Rust` so nginx doesn't send it
//! back here) and returns PHP's answer. It then computes its own answer
//! without writing anything and logs the differences. A method can take
//! traffic once its shadow runs show no differences.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, Method as HttpMethod, StatusCode};
use axum::response::{IntoResponse, Response};
use phab_conduit::call::dispatch;
use phab_conduit::{HttpRequest, Mode};
use phab_parity::{Rules, compare, summarize};
use serde_json::{Value, json};

use crate::{AppState, now};

pub const SKIP_RUST_HEADER: &str = "x-phab-skip-rust";

/// Request headers passed on to PHP.
const FORWARDED_REQUEST_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "cookie",
    "user-agent",
    "x-forwarded-for",
    "x-forwarded-proto",
];

/// Response headers passed back from PHP.
const FORWARDED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "access-control-allow-origin",
    "x-conduit-capabilities",
    "x-frame-options",
    "referrer-policy",
    "cache-control",
    "expires",
    "x-content-type-options",
    "content-security-policy",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub matched: u64,
    pub differed: u64,
    /// Rust would have handed the call to PHP.
    pub skipped: u64,
}

pub struct Shadow {
    methods: HashSet<String>,
    php_url: String,
    client: reqwest::Client,
    counts: Mutex<HashMap<String, Counts>>,
}

impl Shadow {
    pub fn new(methods: impl IntoIterator<Item = String>, php_url: impl Into<String>) -> Self {
        Self {
            methods: methods.into_iter().collect(),
            php_url: php_url.into(),
            client: reqwest::Client::new(),
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// From `PHAB_SHADOW_METHODS` (comma separated) and `PHAB_PHP_URL`.
    pub fn from_env() -> Option<Self> {
        let methods = std::env::var("PHAB_SHADOW_METHODS").ok()?;
        let php_url = std::env::var("PHAB_PHP_URL").ok()?;
        let methods: Vec<String> = methods
            .split(',')
            .map(str::trim)
            .filter(|method| !method.is_empty())
            .map(str::to_owned)
            .collect();
        (!methods.is_empty()).then(|| Self::new(methods, php_url))
    }

    pub fn shadows(&self, method: &str) -> bool {
        self.methods.contains(method)
    }

    pub fn counts(&self) -> Value {
        let counts = self
            .counts
            .lock()
            .map(|counts| counts.clone())
            .unwrap_or_default();
        let mut methods: Vec<&String> = self.methods.iter().collect();
        methods.sort();
        Value::Object(
            methods
                .into_iter()
                .map(|method| {
                    let count = counts.get(method).copied().unwrap_or_default();
                    (
                        method.clone(),
                        json!({
                            "matched": count.matched,
                            "differed": count.differed,
                            "skipped": count.skipped,
                        }),
                    )
                })
                .collect(),
        )
    }

    fn record(&self, method: &str, update: impl FnOnce(&mut Counts)) {
        if let Ok(mut counts) = self.counts.lock() {
            update(counts.entry(method.to_owned()).or_default());
        }
    }
}

pub struct ForwardedRequest {
    pub http_method: HttpMethod,
    pub method: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Forwards the call to PHP, answers with PHP's response and compares it
/// with the Rust answer in the background.
pub async fn handle(
    state: Arc<AppState>,
    request: ForwardedRequest,
    http: HttpRequest,
) -> Response {
    let Some(shadow) = state.shadow.as_ref() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut url = format!(
        "{}/api/{}",
        shadow.php_url.trim_end_matches('/'),
        request.method
    );
    if let Some(query) = &request.query {
        url.push('?');
        url.push_str(query);
    }
    let mut outgoing = shadow
        .client
        .request(request.http_method.clone(), url)
        .header(SKIP_RUST_HEADER, "1")
        .body(request.body.clone());
    for name in FORWARDED_REQUEST_HEADERS {
        for value in request.headers.get_all(*name) {
            outgoing = outgoing.header(*name, value);
        }
    }

    let php = match outgoing.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, method = request.method, "shadow: PHP is unreachable");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let status = php.status();
    let php_headers = php.headers().clone();
    let php_body = match php.bytes().await {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, method = request.method, "shadow: reading PHP's answer");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let compared_state = Arc::clone(&state);
    let compared_body = php_body.clone();
    let method = request.method.clone();
    tokio::spawn(async move {
        compare_with_rust(&compared_state, &method, &http, &compared_body).await;
    });

    let mut response = Response::new(Body::from(php_body));
    *response.status_mut() = status;
    for name in FORWARDED_RESPONSE_HEADERS {
        for value in php_headers.get_all(*name) {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                response.headers_mut().append(name, value.clone());
            }
        }
    }
    response
}

async fn compare_with_rust(state: &AppState, method: &str, http: &HttpRequest, php_body: &[u8]) {
    let Some(shadow) = state.shadow.as_ref() else {
        return;
    };
    let dispatched = dispatch(
        &state.context,
        &state.registry,
        method,
        http,
        Mode::Shadow,
        now(),
    )
    .await;
    let envelope = match dispatched.response {
        phab_conduit::Response::Envelope(envelope) => envelope,
        phab_conduit::Response::Fallback(reason) => {
            tracing::info!(method, reason, "shadow: Rust would hand this call to PHP");
            shadow.record(method, |counts| counts.skipped += 1);
            return;
        }
    };
    let Ok(php_value) = serde_json::from_slice::<Value>(php_body) else {
        tracing::warn!(method, "shadow: PHP's answer is not JSON");
        shadow.record(method, |counts| counts.differed += 1);
        return;
    };
    let differences = compare(&php_value, &envelope.to_value(), &Rules::for_method(method));
    if differences.is_empty() {
        tracing::debug!(method, "shadow: answers match");
        shadow.record(method, |counts| counts.matched += 1);
    } else {
        tracing::warn!(
            method,
            caller = dispatched
                .caller
                .as_ref()
                .map(|user| user.user_name.as_str()),
            differences = summarize(&differences),
            "shadow: answers differ"
        );
        shadow.record(method, |counts| counts.differed += 1);
    }
}
