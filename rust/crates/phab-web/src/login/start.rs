//! The login page, `PhabricatorAuthStartController`: `/auth/start/`,
//! `/login/` and, after a logout, `/auth/loggedout/`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::oauth;
use phab_auth::provider::{self, ProviderConfig, ProviderKind};
use phab_auth::session::{self, SESSION_COOKIE, SessionKind};
use phab_core::{php, random};
use phab_policy::Viewer;

use super::page::{LoginButton, LoginPage, PasswordForm};
use super::{
    CLIENT_ID_COOKIE, Context, INVITE_COOKIE, NEXT_URI_COOKIE, USERNAME_COOKIE, Visitor, respond,
};
use crate::error::PageError;
use crate::state::WebState;

/// `PhabricatorAuthLoginMessageType::MESSAGEKEY`.
const LOGIN_MESSAGE: &str = "auth.login";
/// Length of the `phcid` cookie (`setClientIDCookie()`).
const CLIENT_ID_LENGTH: usize = 16;
/// A next URI cookie younger than this is kept (`setNextURICookie()`).
const NEXT_URI_GRACE: i64 = 120;

/// `GET /auth/start/` and `GET /login/`.
pub async fn start<S: WebState>(
    State(state): State<Arc<S>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(show(state.as_ref(), method, uri, headers, &body, false).await)
}

/// `GET /auth/loggedout/`: the login page without automatic login, so a
/// user who just logged out is not logged back in.
pub async fn logged_out<S: WebState>(
    State(state): State<Arc<S>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(show(state.as_ref(), method, uri, headers, &body, true).await)
}

/// `parseNextURICookie()`: when the cookie was set, `0` for old cookies.
fn next_uri_cookie_time(cookie: &str) -> i64 {
    match cookie.split_once(',') {
        Some((timestamp, _)) => timestamp.trim().parse().unwrap_or(0),
        None => 0,
    }
}

async fn show<S: WebState>(
    state: &S,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
    logged_out: bool,
) -> Result<Response, PageError> {
    let mut context = Context::new(state, method, uri, headers, body).await?;
    context.refuse_restricted_parameters()?;
    if matches!(context.visitor, Visitor::LoggedIn { .. }) {
        return Err(PageError::Fallback("logged in"));
    }
    let request = &context.request;
    if request.is_post() {
        return Err(PageError::Fallback("a form post to the login page"));
    }
    if request.exists("__ajax__") || request.exists("__conduit__") {
        return Err(PageError::Fallback("an Ajax or Conduit request"));
    }
    if request.nonempty_cookie(INVITE_COOKIE).is_some() {
        return Err(PageError::Fallback("an invite"));
    }
    let providers = match provider::load_providers(state.database()).await? {
        phab_auth::Outcome::Done(providers) => providers,
        phab_auth::Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let mut providers: Vec<_> = providers
        .into_iter()
        .filter(|provider| provider.is_enabled && provider.allow_login)
        .collect();
    if providers.is_empty() {
        // First-time setup, or the "no providers" error.
        return Err(PageError::Fallback("no login provider"));
    }
    providers.sort_by_key(|provider| provider.kind.login_order());
    let config = state.config();
    let now = state.now();
    let did_clear = request.str("cleared").is_some_and(php::is_truthy_str);

    context.issue_anonymous_session();
    // A session cookie which loaded no user is stale: clear it.
    if let Visitor::LoggedOut { token } = &context.visitor
        && session::session_kind(token) != SessionKind::Anonymous
    {
        context.request.clear_cookie(config, USERNAME_COOKIE, now);
        context.request.clear_cookie(config, SESSION_COOKIE, now);
        if did_clear {
            let page = LoginPage::error(
                "Authentication Failure",
                "Your login session is invalid, and clearing the session cookie was \
                 unsuccessful. Try clearing your browser cookies.",
            );
            return Ok(context.request.finish(page.render(StatusCode::OK)));
        }
        let target = context.request.uri_with_cleared(true);
        return Ok(context.request.redirect(config, &target));
    }
    if did_clear {
        let target = context.request.uri_with_cleared(false);
        return Ok(context.request.redirect(config, &target));
    }

    if let Some(next) = context.request.nonempty_str("next").map(str::to_owned) {
        let set_at = context
            .request
            .nonempty_cookie(NEXT_URI_COOKIE)
            .map_or(0, next_uri_cookie_time);
        if set_at <= now - NEXT_URI_GRACE {
            context.request.set_temporary_cookie(
                config,
                NEXT_URI_COOKIE,
                &format!("{now},{next}"),
                now,
            );
        }
    }
    if context.request.nonempty_cookie(CLIENT_ID_COOKIE).is_none() {
        let client_id = random::random_characters(CLIENT_ID_LENGTH);
        context
            .request
            .set_temporary_cookie(config, CLIENT_ID_COOKIE, &client_id, now);
    }
    let client_id = context
        .request
        .nonempty_cookie(CLIENT_ID_COOKIE)
        .unwrap_or_default()
        .to_owned();
    let client_state = oauth::client_state(config, &client_id)?;

    // `tryAutoLogin()`.
    if let [only] = providers.as_slice()
        && !logged_out
        && only.kind == ProviderKind::Bmo
        && only.auto_login
    {
        let app = super::provider::bmo_app(config, only)?;
        let target = app.authenticate_uri(oauth::SCOPE, &client_state);
        return Ok(context.request.redirect_external(&target));
    }

    let page = render(&context, &providers, &client_state).await?;
    Ok(context.request.finish(page))
}

/// The login page: the custom message, then the password form and the
/// provider buttons in login order.
async fn render<S: WebState>(
    context: &Context<'_, S>,
    providers: &[ProviderConfig],
    client_state: &str,
) -> Result<Response, PageError> {
    let state = context.state;
    let config = context.config();
    let mut page = LoginPage::new("Login");
    if let Some(text) = phab_auth::user::load_message_text(state.database(), LOGIN_MESSAGE).await? {
        page.custom_message = Some(
            crate::remarkup::render(
                state,
                &Viewer::anonymous(),
                &text,
                phab_remarkup::Mode::Html,
            )
            .await?,
        );
    }
    let scope = context
        .request
        .str("scope")
        .filter(|scope| php::is_truthy_str(scope))
        .unwrap_or(oauth::SCOPE)
        .to_owned();
    for provider in providers {
        match provider.kind {
            ProviderKind::Password => {
                let username = context
                    .request
                    .nonempty_str("username")
                    .or_else(|| context.request.nonempty_cookie(USERNAME_COOKIE))
                    .unwrap_or_default()
                    .to_owned();
                page.password = Some(PasswordForm {
                    action: provider.kind.login_uri(),
                    csrf_token: context.csrf_token().await?,
                    username,
                    allows_registration: provider.allows_registration(),
                });
            }
            ProviderKind::Bmo => {
                let app = super::provider::bmo_app(config, provider)?;
                page.buttons.push(LoginButton {
                    label: if provider.allows_registration() {
                        "Log In or Register"
                    } else {
                        "Log In"
                    },
                    provider: provider.kind.name(),
                    href: app.authenticate_uri(&scope, client_state),
                });
            }
        }
    }
    page.email_login = page.password.is_none();
    Ok(page.render(StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_uri_cookies() {
        assert_eq!(next_uri_cookie_time("1700000000,/D1"), 1_700_000_000);
        assert_eq!(next_uri_cookie_time("/D1"), 0);
        assert_eq!(next_uri_cookie_time("x,/D1"), 0);
    }
}
