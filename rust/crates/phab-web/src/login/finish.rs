//! The end of a login: `PhabricatorAuthValidateController` checks that the
//! browser kept the cookies, `PhabricatorAuthFinishController` turns the
//! partial session into a full one once the user passed multi-factor
//! authentication, or has no factor to pass. Factors other than TOTP are
//! PHP's to prompt for.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::Outcome;
use phab_auth::mfa::{self, HighSecurity, HisecRequest, Mode, Planned};
use phab_auth::session::{self, SessionUser};

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
    // `requireHighSecuritySession()` validates the CSRF token of every
    // POST, after issuing challenges; PHP throws on a bad one. Checking it
    // first writes nothing PHP's replay wouldn't.
    if context.request.is_post() {
        context.validate_csrf().await?;
    }
    // `jump_to_hisec`, read with PHP truthiness.
    let jump_into_hisec = context
        .request
        .cookie(HISEC_COOKIE)
        .is_some_and(phab_core::php::is_truthy_str);
    let now = state.now();
    let info = context.request.info(&context.settings.hostname);
    let request = &context.request;
    let field = |name: &str| request.str(name).map(str::to_owned);
    let hisec = HisecRequest {
        is_post: request.is_post(),
        is_hisec_form: request.exists("__hisec__"),
        field: &field,
    };
    let mode = Mode {
        upgrade_session: true,
        jump_into_hisec,
        workflow_key: mfa::LEGACY_WORKFLOW,
    };
    let planned = match mfa::plan(state.database(), &found, &hisec, mode, now).await? {
        Outcome::Done(planned) => planned,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let outcome = match planned {
        Planned::Token => HighSecurity::Token,
        Planned::Challenge(plan) => mfa::execute(state.database(), *plan, now, &info).await?,
    };
    if let HighSecurity::Prompt { factors, answered } = outcome {
        let page = factor_page(&context, &found, factors, &answered).await?;
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }

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

/// The "Provide Multi-Factor Credentials" dialog, posting back to
/// `/auth/finish/`; cancelling logs out.
async fn factor_page<S: WebState>(
    context: &Context<'_, S>,
    found: &SessionUser,
    factors: Vec<mfa::PromptFactor>,
    answered: &mfa::AnsweredTokens,
) -> Result<LoginPage, PageError> {
    let mut page = LoginPage::new("Provide Multi-Factor Credentials");
    page.factors = Some(super::link::factor_form(
        context.request.uri.path(),
        context.csrf_token().await?,
        Some(found.user.user_name.clone()),
        factors,
        answered,
        "/logout/",
    ));
    Ok(page)
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
