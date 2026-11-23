//! `/search/?query=...` (`PhabricatorSearchController`), the header's
//! search box, as far as it works without the fulltext engine: the jumps
//! of `PhabricatorDatasourceEngine::newJumpURI()` to applications and to
//! revisions by monogram. A query which isn't a jump is a fulltext search,
//! and goes to PHP.

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::response::Redirect;
use phab_core::Phid;

use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorSearchApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorSearchApplication";

/// The jumps of `PhabricatorMonogramDatasourceEngineExtension` and
/// `DiffusionDatasourceEngineExtension` to applications.
fn application_jump(query: &str) -> Option<&'static str> {
    Some(match query.to_ascii_lowercase().as_str() {
        "f" => "/feed/",
        "d" => "/differential/",
        "r" => "/diffusion/",
        "a" => "/diffusion/commit/",
        _ => return None,
    })
}

/// `D123` (`DifferentialRevisionPHIDType::canLoadNamedObject()`).
fn revision_id(query: &str) -> Option<i64> {
    let digits = query.strip_prefix(['D', 'd'])?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `GET /search/`.
pub(crate) async fn search<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(raw): RawQuery,
) -> Result<Redirect, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let mut query = None;
    for (name, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "query" => query = Some(value.into_owned()),
            _ => return Err(PageError::Fallback("a search with options")),
        }
    }
    let query = query
        .filter(|query| !query.is_empty())
        .ok_or(PageError::Fallback("the search page"))?;
    if let Some(uri) = application_jump(&query) {
        return Ok(Redirect::to(uri));
    }
    if let Some(id) = revision_id(&query) {
        let state = state.as_ref();
        let revisions =
            phab_app_differential::storage::load_revisions_by_id(state.database(), &[id]).await?;
        if let Some(revision) = revisions.first() {
            let phid: Phid = revision.phid.clone();
            let handles = harbormaster::load_handles(
                state,
                &Scope::new(state, &viewer),
                std::slice::from_ref(&phid),
            )
            .await?;
            if let Some(uri) = harbormaster::handle(&handles, &phid).uri.clone() {
                return Ok(Redirect::to(&uri));
            }
        }
    }
    Err(PageError::Fallback("a fulltext search"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jumps() {
        assert_eq!(application_jump("F"), Some("/feed/"));
        assert_eq!(application_jump("x"), None);
        assert_eq!(revision_id("D12"), Some(12));
        assert_eq!(revision_id("d3"), Some(3));
        assert_eq!(revision_id("D0"), None);
        assert_eq!(revision_id("D1a"), None);
    }
}
