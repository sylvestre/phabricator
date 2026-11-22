//! The end of a login: `PhabricatorAuthValidateController` checks that the
//! browser kept the cookies, `PhabricatorAuthFinishController` turns the
//! partial session into a full one when the user has no multi-factor
//! authentication to pass. With factors, the prompt is PHP's.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::session;

use super::page::LoginPage;
use super::request::is_valid_local_uri;
use super::{Context, HISEC_COOKIE, NEXT_URI_COOKIE, USERNAME_COOKIE, Visitor, respond};
use crate::error::PageError;
use crate::state::WebState;

/// `/auth/validate/?expect=<username>`.
pub async fn validate<S: WebState>(
    State(state): State<Arc<S>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(show_validate(state.as_ref(), method, uri, headers, &body).await)
}

async fn show_validate<S: WebState>(
    state: &S,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    let mut context = Context::new(state, method, uri, headers, body).await?;
    context.refuse_restricted_parameters()?;
    if let Visitor::LoggedIn { found } = &context.visitor
        && found.user.is_disabled
    {
        return Err(PageError::Fallback("a disabled user"));
    }
    context.issue_anonymous_session();
    let config = context.config();
    let mut failures = Vec::new();
    match context.request.nonempty_str("expect") {
        None => {
            failures.push("Login validation is missing expected parameter (\"phusr\").".to_owned());
        }
        Some(expect) => {
            let actual = context.request.cookie(USERNAME_COOKIE).unwrap_or_default();
            if actual != expect {
                if phab_core::php::is_truthy_str(actual) {
                    failures.push(format!(
                        "Attempted to set 'phusr' cookie to '{expect}', but your browser sent \
                         back a cookie with the value '{actual}'. Clear your browser's cookies \
                         and try again."
                    ));
                } else {
                    failures.push(format!(
                        "Attempted to set 'phusr' cookie to '{expect}', but your browser did \
                         not accept the cookie. Check that cookies are enabled, clear them, and \
                         try again."
                    ));
                }
            }
            if failures.is_empty() && !matches!(context.visitor, Visitor::LoggedIn { .. }) {
                failures.push(
                    "Login cookie was set correctly, but your login session is not valid. Try \
                     clearing cookies and logging in again."
                        .to_owned(),
                );
            }
        }
    }
    if failures.is_empty() {
        return Ok(context.request.redirect(config, "/auth/finish/"));
    }
    let mut page = LoginPage::new("Login Failure");
    page.errors = failures;
    Ok(context.request.finish(page.render(StatusCode::OK)))
}

/// `/auth/finish/`.
pub async fn finish<S: WebState>(
    State(state): State<Arc<S>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(show_finish(state.as_ref(), method, uri, headers, &body).await)
}

/// `getNextURICookie()`: the URI part of `timestamp,/uri`.
fn next_uri(cookie: &str) -> &str {
    cookie.split_once(',').map_or(cookie, |(_, uri)| uri)
}

async fn show_finish<S: WebState>(
    state: &S,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    let mut context = Context::new(state, method, uri, headers, body).await?;
    context.refuse_restricted_parameters()?;
    if context.request.is_post() {
        // Answers to the multi-factor prompt.
        return Err(PageError::Fallback("a multi-factor form"));
    }
    let config = context.config();
    let found = match &context.visitor {
        Visitor::LoggedIn { found } if found.user.is_disabled => {
            return Err(PageError::Fallback("a disabled user"));
        }
        Visitor::LoggedIn { found } if found.session.is_partial => found.clone(),
        _ => {
            context.issue_anonymous_session();
            return Ok(context.request.redirect(config, "/"));
        }
    };
    // `requireHighSecuritySession()` issues a token at once when the user
    // has no factor; otherwise it prompts, which PHP does.
    if phab_auth::user::is_enrolled_in_multi_factor(state.database(), &found.user.phid).await? {
        return Err(PageError::Fallback(
            "multi-factor authentication is pending",
        ));
    }
    let now = state.now();
    let info = context.request.info(&context.settings.hostname);
    session::upgrade_partial_session(state.database(), &found, now, &info).await?;

    let next = context
        .request
        .nonempty_cookie(NEXT_URI_COOKIE)
        .map(next_uri)
        .unwrap_or_default()
        .to_owned();
    context.request.clear_cookie(config, NEXT_URI_COOKIE, now);
    context.request.clear_cookie(config, HISEC_COOKIE, now);
    let next = if is_valid_local_uri(&next) {
        next
    } else {
        "/".to_owned()
    };
    Ok(context.request.redirect(config, &next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_uris_come_from_the_cookie() {
        assert_eq!(next_uri("1700000000,/D1?a=b,c"), "/D1?a=b,c");
        assert_eq!(next_uri("/D1"), "/D1");
    }
}
