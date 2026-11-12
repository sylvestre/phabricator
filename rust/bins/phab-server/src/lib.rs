//! The HTTP side of the Rust service.
//!
//! nginx sends the routes listed in `rust/nginx/phab-rust.conf` here. When a
//! request needs something the Rust code does not implement, the service
//! answers `421 Misdirected Request` with an `X-Phab-Fallback` header and
//! nginx replays the request against PHP.

pub mod shadow;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing};
use phab_auth::session::SESSION_COOKIE;
use phab_conduit::call::Dispatched;
use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, call};
use phab_core::Config;
use phab_db::Database;
use serde_json::json;
use thiserror::Error;
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;

/// Matches `max_allowed_packet` of the MySQL server.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
pub const FALLBACK_HEADER: &str = "x-phab-fallback";

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Config(#[from] phab_core::config::ConfigError),
    #[error(transparent)]
    Database(#[from] phab_db::DbError),
}

pub struct AppState {
    pub context: Context,
    pub registry: Registry,
    pub shadow: Option<shadow::Shadow>,
}

/// Builds the configuration stack like `PhabricatorEnv` (defaults,
/// `conf/local/local.json`, then the database) and connects.
pub async fn build_context(config_dir: &Path, hostname: String) -> Result<Context, StartupError> {
    let mut config = Config::default();
    config.push_local_file(&config_dir.join("local.json"))?;
    let database = Database::connect(&config).await?;
    let entries = database.load_config_entries().await?;
    config.push_layer("Database", entries);
    Ok(Context {
        database,
        config,
        hostname,
    })
}

/// `php_uname('n')`.
pub fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

pub fn default_config_dir() -> PathBuf {
    PathBuf::from("/app/phabricator/conf/local")
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", routing::get(status))
        .route("/api/{method}", routing::any(conduit))
        .layer(RequestDecompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .with_state(state)
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let database = state.context.database.ping().await;
    let code = if database.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = json!({
        "status": if database.is_ok() { "ok" } else { "error" },
        "database": database.map_or_else(|error| error.to_string(), |()| "ok".to_owned()),
        "methods": state.registry.names(),
        "shadow": state.shadow.as_ref().map(shadow::Shadow::counts),
    });
    (code, Json(body)).into_response()
}

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

pub fn fallback_response(reason: &'static str) -> Response {
    let mut response = StatusCode::MISDIRECTED_REQUEST.into_response();
    response
        .headers_mut()
        .insert(FALLBACK_HEADER, HeaderValue::from_static(reason));
    response
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
