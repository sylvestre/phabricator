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

/// Applies `change` to `target`. What the editor leaves to PHP is a
/// fallback, as nothing is written yet, and so are its validation errors,
/// which PHP explains.
pub(crate) async fn apply<S: WebState>(
    state: &S,
    viewer: &WebUser,
    target: Target,
    change: Change,
) -> Result<Outcome, PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let context = web_edit::context(state);
    let request = web_edit::request(state, &context, viewer, Map::new());
    editor::apply(&request, target, change, WEB_SOURCE)
        .await
        .map_err(|error| match error {
            MethodError::Unsupported(reason) => PageError::Fallback(reason),
            MethodError::Conduit { .. } => PageError::Fallback("edits PHP refuses"),
            MethodError::Internal(error) => {
                PageError::Internal(Box::new(std::io::Error::other(error)))
            }
        })
}
