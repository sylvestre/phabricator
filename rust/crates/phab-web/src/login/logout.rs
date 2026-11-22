//! `PhabricatorLogoutController` (`/logout/`): a confirmation dialog, then
//! a form post which ends the session.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::session::{self, SESSION_COOKIE};

use super::page::LoginPage;
use super::{Context, Visitor, respond};
use crate::error::PageError;
use crate::state::WebState;

/// `GET|POST /logout/`.
pub async fn logout<S: WebState>(
    State(state): State<Arc<S>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(handle(state.as_ref(), method, uri, headers, &body).await)
}

/// Whether any provider is configured; PHP warns before logging out of an
/// install without one, which is PHP's page.
async fn has_provider_configs(database: &phab_db::Database) -> Result<bool, PageError> {
    let sql = format!(
        "SELECT 1 FROM {} LIMIT 1",
        database.table("auth", "auth_providerconfig")
    );
    Ok(phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

async fn handle<S: WebState>(
    state: &S,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    let mut context = Context::new(state, method, uri, headers, body).await?;
    context.refuse_restricted_parameters()?;
    // Disabled users and partial sessions may log out too.
    context.issue_anonymous_session();
    let config = context.config();
    let now = state.now();

    if context.is_form_post().await? {
        // The session of the cookie, if it is the visitor's: a session is
        // visible to its owner only.
        if let Visitor::LoggedIn { found } = &context.visitor {
            let info = context.request.info(&context.settings.hostname);
            session::logout_session(state.database(), found, now, &info).await?;
        }
        context.request.clear_cookie(config, SESSION_COOKIE, now);
        return Ok(context.request.redirect(config, "/auth/loggedout/"));
    }

    if matches!(context.visitor, Visitor::LoggedIn { .. }) {
        if !has_provider_configs(state.database()).await? {
            return Err(PageError::Fallback("no login provider is configured"));
        }
        let mut page = LoginPage::new("Log Out?");
        page.logout_csrf = Some(context.csrf_token().await?);
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }
    Ok(context.request.redirect(config, "/"))
}
