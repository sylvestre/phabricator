//! `/harbormaster/build/{restart|pause|resume|abort}/<id>/[<via>/]`
//! (`HarbormasterBuildActionController`): the dialog which confirms a
//! command, and its post, which sends the command to the build
//! (`HarbormasterBuild::sendMessage()`: a message row and the build
//! worker's task; the worker writes the transaction once it applies the
//! message).
//!
//! Left to PHP (`421`, before writing): builds the viewer can't edit
//! (PHP raises a policy exception), commands the build can't take (PHP
//! explains why in a dialog), posts which aren't dialog form posts,
//! workflow requests and invalid CSRF tokens.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_harbormaster::web::{self as harbormaster, Lookup};

use super::{Command, Scope, method_error};
use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// The dialog, as PHP renders `newDialog()` without a workflow.
#[derive(Template)]
#[template(path = "project_dialog.html")]
pub(crate) struct DialogPage {
    shell: Shell,
    title: &'static str,
    paragraphs: Vec<&'static str>,
    link: Option<(&'static str, String)>,
    action: String,
    hidden: Vec<(&'static str, String)>,
    submit: Option<&'static str>,
    cancel: String,
}

/// `newConfirmPromptTitle()` and `newConfirmPromptBody()`.
fn prompt(command: Command) -> (&'static str, &'static str) {
    match command {
        Command::Restart => (
            "Really restart build?",
            "Progress on this build will be discarded and the build will restart. Side effects \
             of the build will occur again. Really restart build?",
        ),
        Command::Pause => (
            "Really pause build?",
            "If you pause this build, work will halt once the current steps complete. You can \
             resume the build later.",
        ),
        Command::Resume => (
            "Really resume build?",
            "Work will continue on the build. Really resume?",
        ),
        Command::Abort => (
            "Really abort build?",
            "Progress on this build will be discarded. Really abort build?",
        ),
    }
}

/// `GET|POST /harbormaster/build/{command}/{id}/[{via}/]`.
async fn handle<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (command, id, via): (Command, i64, Option<&str>),
    method: Method,
    (uri, headers, body): (Uri, HeaderMap, Bytes),
) -> Result<Response, PageError> {
    viewer
        .require_application(state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state, viewer);
    let request = scope.request();
    let build = match harbormaster::build(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(build) => build,
        Lookup::Missing | Lookup::Denied => return Err(PageError::NotFound),
    };
    // The build has the policies of its buildable.
    let buildable =
        harbormaster::buildables_by_phid(&request, std::slice::from_ref(&build.buildable_phid))
            .await
            .map_err(method_error)?
            .pop()
            .ok_or(PageError::Fallback("a visible build without its buildable"))?;
    if !super::buildable::can_edit(state, &scope, &buildable).await? {
        return Err(PageError::Fallback("builds the viewer can't edit"));
    }
    let done = if via == Some("buildable") {
        format!("/B{}", buildable.id)
    } else {
        format!("/harbormaster/build/{id}/")
    };
    let plans = harbormaster::plans_by_phid(&request, std::slice::from_ref(&build.plan_phid))
        .await
        .map_err(method_error)?;
    let builds = [build];
    let states = super::build_states(state, &scope, &builds, &plans).await?;
    if !states[0].can_send(command)? {
        return Err(PageError::Fallback("commands the build can't take"));
    }

    let form = LoginRequest::new(method, uri.clone(), headers, &body);
    if form.is_post() {
        web_edit::check_form_post(state, viewer, &form)?;
        // `isDialogFormPost()`: other posts show the dialog again.
        if matches!(form.str("__dialog__"), None | Some("" | "0")) {
            return Err(PageError::Fallback("posts which aren't dialog posts"));
        }
        if state.config().is_read_only() {
            return Err(PageError::Fallback("writes on a read-only install"));
        }
        let context = web_edit::context(state);
        let request = web_edit::request(state, &context, viewer, serde_json::Map::new());
        harbormaster::send_build_command(&request, &builds[0], command.key())
            .await
            .map_err(method_error)?;
        return Ok(form.redirect(state.config(), &done));
    }
    let (title, body) = prompt(command);
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(DialogPage {
        shell: Shell::for_user(state.config(), viewer, title, current),
        title,
        paragraphs: vec![body],
        link: None,
        action: uri.path().to_owned(),
        hidden: vec![("__dialog__", "1".to_owned())],
        submit: Some(command.build_name()),
        cancel: done,
    })
    .into_response())
}

/// `/harbormaster/build/{first}/{second}/`: a build's run
/// (`{id}/{generation}`, GET only) or a command (`{command}/{id}`).
pub(crate) async fn run_or_command<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((first, second)): Path<(String, String)>,
    query: RawQuery,
    method: Method,
    (uri, headers): (Uri, HeaderMap),
    body: Bytes,
) -> Result<Response, PageError> {
    let Some(command) = Command::from_key(&first) else {
        if method != Method::GET && method != Method::HEAD {
            return Err(PageError::Fallback("posts to build pages"));
        }
        return super::build::show_run(State(state), viewer, Path((first, second)), query, uri)
            .await
            .map(IntoResponse::into_response);
    };
    let id = digits(&second)?;
    handle(
        state.as_ref(),
        &viewer,
        (command, id, None),
        method,
        (uri, headers, body),
    )
    .await
}

/// `/harbormaster/build/{command}/{id}/{via}/`.
pub(crate) async fn command_via<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((command, id, via)): Path<(String, String, String)>,
    method: Method,
    (uri, headers): (Uri, HeaderMap),
    body: Bytes,
) -> Result<Response, PageError> {
    let command = Command::from_key(&command).ok_or(PageError::NotFound)?;
    let id = digits(&id)?;
    handle(
        state.as_ref(),
        &viewer,
        (command, id, Some(via.as_str())),
        method,
        (uri, headers, body),
    )
    .await
}

/// A route's `\d+`: other paths are PHP's 404.
fn digits(text: &str) -> Result<i64, PageError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageError::NotFound);
    }
    text.parse().map_err(|_| PageError::NotFound)
}
