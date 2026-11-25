//! Herald's small forms: `/herald/disable/<id>/<action>/`
//! (`HeraldDisableController`), whose post runs the Rust rule editor
//! ([`phab_app_herald::editor`]), and the object form of the test console
//! (`/herald/test/`, `HeraldTestConsoleController`).

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_herald::editor::{Change, Target};
use phab_remarkup::Mode;

use super::APPLICATION;
use super::rule::{can_edit, visible_rules};
use crate::error::PageError;
use crate::harbormaster::Scope;
use crate::login::request::LoginRequest;
use crate::project_edit::DialogPage;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// `GET|POST /herald/disable/{id}/{action}/`: "Really disable this
/// rule?", or enable it for any other action; the post disables or enables
/// it and goes back to the rule (`setContinueOnNoEffect(true)`: a post
/// without effect is left to PHP).
pub(crate) async fn disable<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((id, action)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    // `(?P<id>[1-9]\d*)/(?P<action>\w+)/`.
    let id: i64 = id
        .strip_prefix(|first: char| ('1'..='9').contains(&first))
        .filter(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|_| id.parse().ok())
        .ok_or(PageError::NotFound)?;
    if action.is_empty()
        || !action
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(PageError::NotFound);
    }
    viewer.require_application(&*state, APPLICATION).await?;
    let state = state.as_ref();
    let rule = phab_app_herald::web::load_rules(
        state.database(),
        &phab_app_herald::web::RuleFilter {
            ids: Some(&[id]),
            ..phab_app_herald::web::RuleFilter::default()
        },
    )
    .await?
    .pop()
    .ok_or(PageError::NotFound)?;
    if rule.is_object_rule() {
        return Err(PageError::Fallback("an object Herald rule"));
    }
    let scope = Scope::new(state, &viewer);
    // `requireCapabilities()` with `executeOne()` raises the policy
    // exception, and so does the global rules capability.
    let rule = visible_rules(state, &scope, vec![rule])
        .await?
        .pop()
        .ok_or(PageError::PolicyDenied)?;
    if !can_edit(state, &scope, &rule).await? {
        return Err(PageError::PolicyDenied);
    }
    let is_disable = action == "disable";
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if request.is_post() {
        web_edit::check_form_post(state, &viewer, &request)?;
        let change = Change::Disable(is_disable);
        super::edit::apply(state, &viewer, Target::Rule(rule.id), change).await?;
        let next = format!("/{}", rule.monogram());
        return Ok(request.redirect(state.config(), &next));
    }
    let (title, body, button) = if is_disable {
        (
            "Really disable this rule?",
            "This rule will no longer activate.",
            "Disable Rule",
        )
    } else {
        (
            "Really enable this rule?",
            "This rule will become active again.",
            "Enable Rule",
        )
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(DialogPage::new(
        Shell::for_user(state.config(), &viewer, title, current),
        title,
        vec![body],
        uri.path().to_owned(),
        Some(button),
        format!("/{}", rule.monogram()),
    ))
    .into_response())
}

/// The instructions of the test console, in Remarkup.
const TEST_INSTRUCTIONS: &str = "Enter an object to test rules for, like a Diffusion commit (e.g., \
                                 `rX123`) or a Differential revision (e.g., `D123`). You will be \
                                 shown the results of a dry run on the object.";

#[derive(Template)]
#[template(path = "herald_test.html")]
pub(crate) struct TestConsolePage {
    shell: Shell,
    instructions_html: String,
}

/// `GET /herald/test/`: the form asking for the object to test rules on.
/// PHP only reads the object name from a post.
pub(crate) async fn test_console<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<TestConsolePage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let markup = crate::remarkup::render(
        state.as_ref(),
        &viewer.user.viewer(),
        TEST_INSTRUCTIONS,
        Mode::Html,
    )
    .await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(TestConsolePage {
        shell: Shell::for_user(state.config(), &viewer, "Test Console", current),
        instructions_html: String::from_utf8_lossy(markup.as_bytes()).into_owned(),
    }))
}
