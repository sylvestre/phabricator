//! Running the Rust rule editor ([`phab_app_herald::editor`]) for the
//! Herald forms, as the viewer, with a web content source.

use phab_app_herald::editor::{self, Change, Outcome, Target};
use phab_conduit::MethodError;
use phab_db::editor::WEB_SOURCE;
use serde_json::Map;

use crate::error::PageError;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// Why the editor wrote nothing.
pub(crate) enum Refusal {
    /// What it leaves to PHP (a fallback: nothing is written yet), or a
    /// failure.
    Page(PageError),
    /// `PhabricatorApplicationTransactionValidationException::getMessage()`,
    /// which the rule form shows.
    Invalid(String),
}

/// Applies `change` to `target`. Validation errors are left to PHP, which
/// explains them.
pub(crate) async fn apply<S: WebState>(
    state: &S,
    viewer: &WebUser,
    target: Target,
    change: Change,
) -> Result<Outcome, PageError> {
    match apply_validated(state, viewer, target, change).await {
        Ok(outcome) => Ok(outcome),
        Err(Refusal::Page(error)) => Err(error),
        Err(Refusal::Invalid(_)) => Err(PageError::Fallback("edits PHP refuses")),
    }
}

/// Applies `change` to `target`, returning the validation errors.
pub(crate) async fn apply_validated<S: WebState>(
    state: &S,
    viewer: &WebUser,
    target: Target,
    change: Change,
) -> Result<Outcome, Refusal> {
    if state.config().is_read_only() {
        return Err(Refusal::Page(PageError::Fallback(
            "writes on a read-only install",
        )));
    }
    let context = web_edit::context(state);
    let request = web_edit::request(state, &context, viewer, Map::new());
    editor::apply(&request, target, change, WEB_SOURCE)
        .await
        .map_err(|error| match error {
            MethodError::Unsupported(reason) => Refusal::Page(PageError::Fallback(reason)),
            MethodError::Conduit { info, .. } => Refusal::Invalid(info.unwrap_or_default()),
            MethodError::Internal(error) => {
                Refusal::Page(PageError::Internal(Box::new(std::io::Error::other(error))))
            }
        })
}
