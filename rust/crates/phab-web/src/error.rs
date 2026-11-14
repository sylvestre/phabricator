//! Why a page could not be shown, and the answers that go with it.

use askama::Template;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::response;
use crate::shell::Shell;

/// Holds the reason of a `421` answer, which nginx replays against PHP.
pub const FALLBACK_HEADER: &str = "x-phab-fallback";

/// The `421 Misdirected Request` answer which sends the request to PHP.
pub fn fallback_response(reason: &'static str) -> Response {
    let mut response = StatusCode::MISDIRECTED_REQUEST.into_response();
    response
        .headers_mut()
        .insert(FALLBACK_HEADER, HeaderValue::from_static(reason));
    response
}

/// Why a page is not shown.
#[derive(Debug)]
pub enum PageError {
    /// PHP must answer: the request needs something Rust doesn't implement.
    Fallback(&'static str),
    /// The object or page does not exist (`Aphront404Response`).
    NotFound,
    /// The viewer may not see the object
    /// (`PhabricatorPolicyException`).
    PolicyDenied,
    /// The request is wrong; the message says how.
    BadRequest(&'static str),
    /// PHP refused a write; its message is meant for the user.
    Conduit(crate::php::ConduitError),
    /// Something failed; the details are logged, not shown.
    Internal(Box<dyn std::error::Error + Send + Sync>),
}

impl From<phab_auth::AuthError> for PageError {
    fn from(error: phab_auth::AuthError) -> Self {
        Self::Internal(Box::new(error))
    }
}

impl From<phab_db::DbError> for PageError {
    fn from(error: phab_db::DbError) -> Self {
        Self::Internal(Box::new(error))
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    shell: Shell,
    heading: &'static str,
    message: String,
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        let (status, heading, message) = match self {
            Self::Fallback(reason) => return fallback_response(reason),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "404 Not Found",
                "The page you requested was not found.".to_owned(),
            ),
            Self::PolicyDenied => (
                StatusCode::FORBIDDEN,
                "Access Denied",
                "You do not have permission to view this object.".to_owned(),
            ),
            Self::BadRequest(message) => {
                (StatusCode::BAD_REQUEST, "Bad Request", message.to_owned())
            }
            Self::Conduit(crate::php::ConduitError::Method { info, .. }) => {
                (StatusCode::BAD_REQUEST, "Unable to Save", info)
            }
            // 503 and not 502 or 504, which nginx would replay against PHP
            // and show PHP's 404 instead of this page.
            Self::Conduit(crate::php::ConduitError::Transport(error)) if error.is_timeout() => {
                tracing::error!(%error, "write through PHP timed out");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to Save",
                    "Phabricator took too long to answer, and the change may have been \
                     saved: reload the revision before trying again."
                        .to_owned(),
                )
            }
            Self::Conduit(error) => {
                tracing::error!(%error, "write through PHP failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to Save",
                    "Phabricator could not be reached; try again.".to_owned(),
                )
            }
            Self::Internal(error) => {
                tracing::error!(%error, "page failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unhandled Exception",
                    "Something went wrong while building this page.".to_owned(),
                )
            }
        };
        let page = ErrorPage {
            shell: Shell::bare(heading),
            heading,
            message,
        };
        response::render(status, &page)
    }
}
