//! The HTTP routes: `/status`, the Conduit endpoint and the web pages.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing};
use phab_auth::session::SESSION_COOKIE;
use phab_conduit::call::Dispatched;
use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, call};
use serde_json::json;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::shadow;

pub use phab_web::fallback_response;

/// Matches `max_allowed_packet` of the MySQL server.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How long a request may take: below nginx's 60 second
/// `proxy_read_timeout`, and above the PHP client's 45 seconds, so a write
/// that times out still gets its own error page.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(55);

/// What the handlers share.
pub struct AppState {
    /// The configuration, database and host name.
    pub context: Context,
    /// The Conduit methods Rust implements.
    pub registry: Registry,
    /// Shadow mode, if enabled.
    pub shadow: Option<shadow::Shadow>,
    /// The Remarkup engine of the pages.
    pub remarkup: phab_remarkup::Engine,
    /// PHP's Conduit API, for the writes of the pages.
    pub php: Option<phab_web::php::PhpConduit>,
    /// Web login, when `PHAB_RUST_LOGIN=1`.
    pub login: Option<phab_web::login::LoginSettings>,
}

impl phab_web::WebState for AppState {
    fn database(&self) -> &phab_db::Database {
        &self.context.database
    }

    fn config(&self) -> &phab_core::Config {
        &self.context.config
    }

    fn now(&self) -> i64 {
        now()
    }

    fn remarkup(&self) -> &phab_remarkup::Engine {
        &self.remarkup
    }

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        self.php.as_ref()
    }

    fn hostname(&self) -> Option<&str> {
        Some(&self.context.hostname)
    }

    fn login(&self) -> Option<&phab_web::login::LoginSettings> {
        self.login.as_ref()
    }
}

/// The current Unix time in seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The service's routes.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", routing::get(status))
        .route("/api/{method}", routing::any(conduit))
        .merge(phab_web::router::<AppState>())
        .layer(RequestDecompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        // 503 rather than 504, which nginx would replay against PHP.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            REQUEST_TIMEOUT,
        ))
        // A panic fails its own request with a 500, not the connection.
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let database = state.context.database.ping().await;
    let code = if database.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    // The error names the server and the account: it goes to the log only.
    if let Err(error) = &database {
        tracing::error!(%error, "status: database ping failed");
    }
    let body = json!({
        "status": if database.is_ok() { "ok" } else { "error" },
        "database": if database.is_ok() { "ok" } else { "unreachable" },
        "methods": state.registry.names(),
        "shadow": state.shadow.as_ref().map(shadow::Shadow::counts),
    });
    (code, Json(body)).into_response()
}

/// The parts of an HTTP request Conduit reads, including the session cookie.
pub fn http_request(headers: &HeaderMap, query: Option<String>, body: &Bytes) -> HttpRequest {
    let session_cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_owned());
    HttpRequest {
        content_type: headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        query: query.unwrap_or_default(),
        body: String::from_utf8_lossy(body).into_owned(),
        session_cookie,
    }
}

/// The headers `AphrontJSONResponse` sends for Conduit answers.
pub fn envelope_response(envelope: &Envelope) -> Response {
    let mut response = Response::new(Body::from(envelope.to_body()));
    let headers = response.headers_mut();
    for (name, value) in [
        ("x-frame-options", "Deny"),
        ("referrer-policy", "no-referrer"),
        ("content-type", "application/json"),
        ("x-conduit-capabilities", "gzip"),
        ("access-control-allow-origin", "*"),
        ("cache-control", "no-store"),
        ("expires", "Sat, 01 Jan 2000 00:00:00 GMT"),
        ("x-content-type-options", "nosniff"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

async fn conduit(
    State(state): State<Arc<AppState>>,
    http_method: axum::http::Method,
    UrlPath(method): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = http_request(&headers, query.clone(), &body);
    if state
        .shadow
        .as_ref()
        .is_some_and(|shadow| shadow.shadows(&method))
    {
        let forwarded = shadow::ForwardedRequest {
            http_method,
            method,
            query,
            headers,
            body,
        };
        return shadow::handle(state, forwarded, request).await;
    }
    let Dispatched { response, .. } = call::dispatch(
        &state.context,
        &state.registry,
        &method,
        &request,
        Mode::Serve,
        now(),
    )
    .await;
    match response {
        phab_conduit::Response::Envelope(envelope) => envelope_response(&envelope),
        phab_conduit::Response::Fallback(reason) => {
            tracing::info!(method, reason, "handing the request to PHP");
            fallback_response(reason)
        }
    }
}
