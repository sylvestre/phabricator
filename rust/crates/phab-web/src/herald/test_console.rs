//! `/herald/test/` (`HeraldTestConsoleController`): the object form, the
//! choice of event for the object, and the dry run of the rules against
//! a revision ([`phab_app_differential::edit::herald_dry_run()`]), which
//! writes a transcript and goes to it. Objects other than revisions
//! (`D123`) are left to PHP.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_differential::storage;
use phab_conduit::MethodError;
use phab_remarkup::Mode;
use serde_json::Map;

use super::APPLICATION;
use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// The instructions of the test console, in Remarkup.
const TEST_INSTRUCTIONS: &str = "Enter an object to test rules for, like a Diffusion commit (e.g., \
                                 `rX123`) or a Differential revision (e.g., `D123`). You will be \
                                 shown the results of a dry run on the object.";

#[derive(Template)]
#[template(path = "herald_test.html")]
pub(crate) struct TestConsolePage {
    shell: Shell,
    instructions_html: String,
    errors: Vec<&'static str>,
    name_error: Option<&'static str>,
    object_name: String,
    /// The event to choose, once the object is known: its name and test
    /// description.
    adapter: Option<(&'static str, &'static str)>,
}

/// The revision ID of a monogram like `D123`
/// (`DifferentialRevisionPHIDType`); other names are left to PHP.
fn revision_id(name: &str) -> Option<i64> {
    let digits = name.strip_prefix('D')?;
    (digits.starts_with(|first: char| ('1'..='9').contains(&first))
        && digits.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| digits.parse().ok())
    .flatten()
}

/// Whether the viewer sees revision `id`.
async fn sees_revision<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<bool, PageError> {
    let revisions = storage::load_revisions_by_id(state.database(), &[id]).await?;
    Ok(!storage::visible_revisions(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        revisions,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?
    .is_empty())
}

/// `GET|POST /herald/test/`.
pub(crate) async fn test_console<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let state = state.as_ref();
    let markup =
        crate::remarkup::render(state, &viewer.user.viewer(), TEST_INSTRUCTIONS, Mode::Html)
            .await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    let mut page = TestConsolePage {
        shell: Shell::for_user(state.config(), &viewer, "Test Console", current),
        instructions_html: String::from_utf8_lossy(markup.as_bytes()).into_owned(),
        errors: Vec::new(),
        name_error: None,
        object_name: String::new(),
        adapter: None,
    };
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if !request.is_post() {
        return Ok(Page(page).into_response());
    }
    // `isFormPost()`.
    web_edit::check_form_post(state, &viewer, &request)?;
    let name = request
        .str("object_name")
        .unwrap_or_default()
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
        .to_owned();
    page.object_name.clone_from(&name);
    // `if (!$v_name)`: "0" is no name either.
    if !phab_core::php::is_truthy_str(&name) {
        page.name_error = Some("Required");
        page.errors.push("An object name is required.");
        return Ok(Page(page).into_response());
    }
    let id = revision_id(&name).ok_or(PageError::Fallback("objects other than revisions"))?;
    if !sees_revision(state, &viewer, id).await? {
        page.name_error = Some("Invalid");
        page.errors.push("No object exists with that name.");
        return Ok(Page(page).into_response());
    }
    // Only the revision adapter tests revisions.
    if request.str("adapter") != Some("differential") {
        page.adapter = Some((
            "Differential Revisions",
            "Test rules which run when a revision is created or updated.",
        ));
        return Ok(Page(page).into_response());
    }
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let context = web_edit::context(state);
    let api = web_edit::request(state, &context, &viewer, Map::new());
    let transcript = phab_app_differential::edit::herald_dry_run(&api, id)
        .await
        .map_err(|error| match error {
            MethodError::Unsupported(reason) => PageError::Fallback(reason),
            MethodError::Conduit { .. } => PageError::Fallback("dry runs PHP refuses"),
            MethodError::Internal(error) => {
                PageError::Internal(Box::new(std::io::Error::other(error)))
            }
        })?;
    Ok(request.redirect(state.config(), &format!("/herald/transcript/{transcript}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_revision_names_like_php() {
        assert_eq!(revision_id("D12"), Some(12));
        assert_eq!(revision_id("D0"), None);
        assert_eq!(revision_id("d12"), None);
        assert_eq!(revision_id("rX1"), None);
    }
}
