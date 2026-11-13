//! HTML answers and the headers that go with them.

use askama::Template;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Scripts, styles and forms only come from the service itself; pages
/// can't be framed.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data: https:; \
     style-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'; \
     form-action 'self'; base-uri 'none'";

/// An HTML page with the headers every page gets: no caching (pages show
/// what the viewer may see), no framing, no MIME sniffing.
pub fn html_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/html; charset=UTF-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::X_FRAME_OPTIONS, "Deny"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Renders a template; a template that fails to render is a bug, answered
/// with a plain 500.
pub(crate) fn render(status: StatusCode, template: &impl Template) -> Response {
    match template.render() {
        Ok(body) => html_response(status, body),
        Err(error) => {
            tracing::error!(%error, "template failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A rendered page: a template answered with `200 OK`.
pub(crate) struct Page<T>(pub T);

impl<T: Template> IntoResponse for Page<T> {
    fn into_response(self) -> Response {
        render(StatusCode::OK, &self.0)
    }
}
