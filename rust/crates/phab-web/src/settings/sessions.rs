//! The actions of the Sessions panel:
//! `PhabricatorAuthTerminateSessionController`
//! (`/auth/session/terminate/<id|all>/`), which deletes the viewer's other
//! sessions, and `PhabricatorAuthDowngradeSessionController`
//! (`/auth/session/downgrade/`), which takes the viewer's session out of
//! high security. Neither requires high security.
//!
//! Left to PHP (`421`): workflow (Ajax) requests and viewers with sessions
//! of external accounts, which the panel doesn't list either.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use phab_app_settings::account;
use phab_auth::session_actions;

use super::dialog::{self, Dialog, Paragraph};
use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorAuthApplication`.
const APPLICATION: &str = "PhabricatorAuthApplication";
/// Where both actions go back to.
const PANEL_URI: &str = "/settings/panel/sessions/";

async fn begin<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
) -> Result<(), PageError> {
    viewer.require_application(state, APPLICATION).await?;
    if request.exists("__ajax__") || request.exists("__wflow__") {
        return Err(PageError::Fallback("a workflow request"));
    }
    Ok(())
}

/// `GET|POST /auth/session/terminate/<id>/`, `<id>` being a session ID
/// or `all`.
pub(crate) async fn terminate<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let request = LoginRequest::new(method, uri, headers, &body);
    begin(state, &viewer, &request).await?;
    let is_all = id == "all";
    let mut sessions = account::sessions(state.database(), &viewer.user.phid).await?;
    if !is_all {
        // `withIDs(array($id))`, which `%Ld` casts to an integer.
        let id = phab_app_settings::preferences::int_cast_str(&id);
        sessions.retain(|session| session.id == id);
    }
    // The current login session is never terminated.
    sessions.retain(|session| session.id != viewer.session.id);
    if sessions.is_empty() {
        let dialog = Dialog::new("No Matching Sessions")
            .paragraph("There are no matching sessions to terminate.")
            .paragraph(
                "(You can not terminate your current login session. To terminate it, log out.)",
            )
            .cancel(PANEL_URI, "Cancel");
        return Ok(dialog.render(state, &viewer, request));
    }
    if dialog::is_dialog_form_post(state, &viewer, &request)? {
        dialog::require_writable(state)?;
        let ids: Vec<i64> = sessions.iter().map(|session| session.id).collect();
        session_actions::terminate(state.database(), &ids).await?;
        return Ok(request.redirect(state.config(), PANEL_URI));
    }
    let mut dialog = Dialog::new(if is_all {
        "Terminate Sessions?"
    } else {
        "Terminate Session?"
    })
    .submit("Terminate")
    .cancel(PANEL_URI, "Cancel");
    if is_all {
        dialog = dialog.paragraph(
            "Really terminate all sessions? (Your current login session will not be \
             terminated.)",
        );
    } else {
        let prefix = sessions
            .last()
            .map(|session| session.key_prefix.clone())
            .unwrap_or_default();
        dialog
            .paragraphs
            .push(Paragraph::bold("Really terminate session ", prefix, "?"));
    }
    Ok(dialog.render(state, &viewer, request))
}

/// `GET|POST /auth/session/downgrade/`: leaves high security.
pub(crate) async fn downgrade<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let request = LoginRequest::new(method, uri, headers, &body);
    begin(state, &viewer, &request).await?;
    let now = state.now();
    // `getHighSecurityUntil() < time()`, `null` being smaller.
    if viewer
        .session
        .high_security_until
        .is_none_or(|until| until < now)
    {
        let dialog = Dialog::new("Normal Security Restored")
            .paragraph("Your session is no longer in high security.")
            .cancel(PANEL_URI, "Continue");
        return Ok(dialog.render(state, &viewer, request));
    }
    if dialog::is_form_post(state, &viewer, &request)? {
        dialog::require_writable(state)?;
        let info = request.info(state.hostname().unwrap_or_default());
        session_actions::exit_high_security(state.database(), &viewer.session_user(), &info, now)
            .await?;
        return Ok(request.redirect(state.config(), "/auth/session/downgrade/"));
    }
    let dialog = Dialog::new("Leaving High Security")
        .paragraph("Leave high security and return your session to normal security levels?")
        .paragraph(
            "If you leave high security, you will need to authenticate again the next time you \
             try to take a high security action.",
        )
        .paragraph("On the plus side, that purple notification bubble will disappear.")
        .submit("Leave High Security")
        .cancel(PANEL_URI, "Stay");
    Ok(dialog.render(state, &viewer, request))
}
