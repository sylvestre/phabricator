//! The actions of the Conduit API Tokens panel:
//! `PhabricatorConduitTokenEditController` (`/conduit/token/edit/`
//! generates a token, `/conduit/token/edit/<id>/` shows one) and
//! `PhabricatorConduitTokenTerminateController`
//! (`/conduit/token/terminate/[<id>/]` expires one or all). Both require
//! high security, as PHP does, before showing or writing anything.
//!
//! Tokens of the viewer are ported, and those of bots for administrators
//! (whose `CAN_EDIT` policy is the administrators'). Left to PHP (`421`):
//! workflow (Ajax) requests, tokens of other users, mailing lists and
//! other objects, which PHP refuses or answers with its own pages.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use phab_auth::User;
use phab_auth::conduit_token::{self, LiveToken, TYPE_CLUSTER};
use phab_core::Phid;

use super::dialog::{self, Dialog, Hisec, TextField};
use super::panel_uri;
use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorConduitApplication`.
const APPLICATION: &str = "PhabricatorConduitApplication";

/// What every action checks first.
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

/// The user a token is for, if the viewer may edit their tokens: the
/// viewer, or a bot for an administrator (`CAN_VIEW` and `CAN_EDIT` of
/// `PhabricatorUser`).
async fn token_owner<S: WebState>(
    state: &S,
    viewer: &WebUser,
    phid: Option<&str>,
) -> Result<User, PageError> {
    let phid = phid
        .filter(|phid| !phid.is_empty())
        .ok_or(PageError::Fallback("a token for no object"))?;
    if phid == viewer.user.phid.as_str() {
        return Ok(viewer.user.clone());
    }
    let user = phab_auth::user::load_users_by_phid(state.database(), &[Phid::new(phid)])
        .await?
        .into_iter()
        .next()
        .ok_or(PageError::Fallback(
            "the tokens of an object other than a user",
        ))?;
    if viewer.user.is_admin && user.is_system_agent && !user.is_mailing_list {
        return Ok(user);
    }
    Err(PageError::Fallback("the tokens of another user"))
}

/// The Conduit API Tokens panel of `user`.
fn tokens_panel_uri(user: &User) -> String {
    panel_uri(&user.user_name, "apitokens")
}

/// A token ID from the route (`\d+`), which PHP doesn't route otherwise.
fn route_id(id: &str) -> Result<i64, PageError> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageError::Fallback("a route PHP doesn't have"));
    }
    id.parse().map_err(|_| PageError::NotFound)
}

/// A live token the viewer may edit, `404` when there is none.
async fn load_token<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<(LiveToken, User), PageError> {
    let token = conduit_token::load_live(state.database(), id, state.now())
        .await?
        .ok_or(PageError::NotFound)?;
    let owner = token_owner(state, viewer, Some(token.object_phid.as_str())).await?;
    Ok((token, owner))
}

/// `GET|POST /conduit/token/edit/`: generates a token for `objectPHID`.
pub(crate) async fn generate<S: WebState>(
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
    let owner = token_owner(state, &viewer, request.str("objectPHID")).await?;
    let panel_uri = tokens_panel_uri(&owner);
    let request = match dialog::require_high_security(state, &viewer, request, &panel_uri).await? {
        Hisec::Granted(request) => request,
        Hisec::Prompt(response) => return Ok(response),
    };
    if dialog::is_form_post(state, &viewer, &request)? {
        dialog::require_writable(state)?;
        let id = conduit_token::create_standard(state.database(), &owner.phid, state.now()).await?;
        return Ok(request.redirect(state.config(), &format!("/conduit/token/edit/{id}/")));
    }
    let dialog = Dialog::new("Generate API Token")
        .hidden("objectPHID", owner.phid.as_str())
        .paragraph("Generate a new API token?")
        .submit("Generate Token")
        .cancel(panel_uri, "Cancel");
    Ok(dialog.render(state, &viewer, request))
}

/// `GET|POST /conduit/token/edit/<id>/`: shows a token.
pub(crate) async fn view<S: WebState>(
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
    let (token, owner) = load_token(state, &viewer, route_id(&id)?).await?;
    let panel_uri = tokens_panel_uri(&owner);
    let request = match dialog::require_high_security(state, &viewer, request, &panel_uri).await? {
        Hisec::Granted(request) => request,
        Hisec::Prompt(response) => return Ok(response),
    };
    if dialog::is_form_post(state, &viewer, &request)? {
        // `$token->save()`, which changes nothing else.
        dialog::require_writable(state)?;
        conduit_token::touch(state.database(), &token, state.now()).await?;
        return Ok(request.redirect(state.config(), &panel_uri));
    }
    let mut dialog = Dialog::new("View API Token")
        .hidden("objectPHID", owner.phid.as_str())
        .cancel(panel_uri, "Done");
    if token.token_type == TYPE_CLUSTER {
        dialog = dialog.paragraph(
            "This token is automatically generated, and used to make requests between nodes \
             in a cluster. You can not use this token in external applications.",
        );
    } else {
        dialog.field = Some(TextField {
            label: "Token",
            name: "token",
            value: token.token.expose().to_owned(),
            error: None,
            read_only: true,
        });
    }
    Ok(dialog.render(state, &viewer, request))
}

/// `GET|POST /conduit/token/terminate/`: expires every live token of
/// `objectPHID`.
pub(crate) async fn terminate_all<S: WebState>(
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
    let owner = token_owner(state, &viewer, request.str("objectPHID")).await?;
    let tokens =
        conduit_token::load_live_for_object(state.database(), &owner.phid, state.now()).await?;
    let text = (
        "Terminate API Tokens",
        "Really terminate all active API tokens? Any systems using these tokens will no longer \
         be able to make API requests.",
        "Terminate Tokens",
    );
    terminate_tokens(state, &viewer, request, (owner, tokens), text).await
}

/// `GET|POST /conduit/token/terminate/<id>/`: expires one token.
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
    let (token, owner) = load_token(state, &viewer, route_id(&id)?).await?;
    let text = (
        "Terminate API Token",
        "Really terminate this token? Any system using this token will no longer be able to \
         make API requests.",
        "Terminate Token",
    );
    terminate_tokens(state, &viewer, request, (owner, vec![token]), text).await
}

/// What both terminations do once they found the tokens.
async fn terminate_tokens<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: LoginRequest,
    (owner, tokens): (User, Vec<LiveToken>),
    (title, body, submit): (&'static str, &'static str, &'static str),
) -> Result<Response, PageError> {
    let panel_uri = tokens_panel_uri(&owner);
    let request = match dialog::require_high_security(state, viewer, request, &panel_uri).await? {
        Hisec::Granted(request) => request,
        Hisec::Prompt(response) => return Ok(response),
    };
    if tokens.is_empty() {
        let dialog = Dialog::new("No Tokens to Terminate")
            .paragraph("There are no API tokens to terminate.")
            .cancel(panel_uri, "Cancel");
        return Ok(dialog.render(state, viewer, request));
    }
    if dialog::is_form_post(state, viewer, &request)? {
        dialog::require_writable(state)?;
        conduit_token::terminate(state.database(), &tokens, state.now()).await?;
        return Ok(request.redirect(state.config(), &panel_uri));
    }
    let dialog = Dialog::new(title)
        .hidden("objectPHID", owner.phid.as_str())
        .paragraph(body)
        .submit(submit)
        .cancel(panel_uri, "Cancel");
    Ok(dialog.render(state, viewer, request))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_ids_are_digits() {
        assert_eq!(route_id("12").unwrap(), 12);
        assert!(matches!(route_id("1a"), Err(PageError::Fallback(_))));
        assert!(matches!(route_id(""), Err(PageError::Fallback(_))));
    }
}
