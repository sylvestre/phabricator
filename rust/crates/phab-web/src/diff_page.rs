//! The diff pages: `/differential/diff/<id>/` (`DifferentialDiffViewController`),
//! a diff not attached to a revision yet (diffs of revisions redirect to
//! the revision, which shows them with `?id=`), and
//! `/differential/diff/<id>/changesets/` (`DifferentialChangesetListController`),
//! the builtin query of `DifferentialChangesetSearchEngine` listing a
//! diff's files.
//!
//! The diff page's "Attach To" form posts to PHP (`421`): attaching runs
//! the revision editor. Left to PHP too: saved and custom changeset
//! queries.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_app_differential::diff_search::{self, DiffLookup, VisibleDiff};
use phab_app_differential::storage;
use phab_diff::{ChangeType, Changeset};
use phab_policy::application;

use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

/// `PhutilUTF8StringTruncator` limit of the revision names of the form.
const REVISION_NAME_GLYPHS: usize = 128;

/// A file of the diff.
pub(crate) struct FileItem {
    id: i64,
    path: String,
    change: &'static str,
    added: i64,
    removed: i64,
}

impl FileItem {
    fn new(changeset: &Changeset) -> Self {
        Self {
            id: changeset.id,
            path: changeset.display_filename(),
            change: change_name(changeset.change_type),
            added: changeset.add_lines,
            removed: changeset.del_lines,
        }
    }
}

/// A revision the diff may update.
pub(crate) struct RevisionOption {
    id: i64,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "diff_page.html")]
pub(crate) struct DiffPage {
    shell: Shell,
    id: i64,
    revisions: Vec<RevisionOption>,
    files: Vec<FileItem>,
    changesets: Vec<String>,
}

#[derive(Template)]
#[template(path = "diff_changesets.html")]
pub(crate) struct ChangesetList {
    shell: Shell,
    diff_id: i64,
    revision_id: Option<i64>,
    files: Vec<FileItem>,
    next: Option<i64>,
}

/// `DifferentialChangeType::getFullNameForChangeType()`.
pub(crate) fn change_name(change_type: ChangeType) -> &'static str {
    match change_type.value() {
        1 => "Added",
        2 => "Modified",
        3 => "Deleted",
        4 => "Moved Away",
        5 => "Copied Away",
        6 => "Moved Here",
        7 => "Copied Here",
        8 => "Deleted After Multiple Copy",
        9 => "Commit Message",
        10 => "Contents Modified",
        _ => "Unknown",
    }
}

/// The ID of a route's `{id}` (`\d+`); anything else is PHP's 404.
fn parse_id(id: &str) -> Result<i64, PageError> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageError::NotFound);
    }
    id.parse().map_err(|_| PageError::NotFound)
}

/// The diff `id`, as `DifferentialDiffQuery::executeOne()` loads it for
/// the viewer.
pub(crate) async fn load_diff<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<VisibleDiff, PageError> {
    let scope = Scope::new(state, viewer);
    match diff_search::lookup(&scope.request(), id)
        .await
        .map_err(harbormaster::method_error)?
    {
        DiffLookup::Visible(diff) => Ok(diff),
        DiffLookup::NotFound => Err(PageError::NotFound),
        DiffLookup::Denied => Err(PageError::PolicyDenied),
    }
}

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// An `AphrontRedirectResponse` to a local path.
fn redirect<S: WebState>(state: &S, path: &str) -> Response {
    let location = state.config().uri(path).unwrap_or_else(|_| path.to_owned());
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

/// `PhutilUTF8StringTruncator`: `glyphs` characters at most, the last one
/// an ellipsis when cut.
fn truncate(text: &str, glyphs: usize) -> String {
    if text.chars().count() <= glyphs {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(glyphs - 1).collect();
    cut.push('…');
    cut
}

/// `loadSelectableRevisions()`: the viewer's open revisions they may edit,
/// most recently updated first, and the selected one (`?revisionID=`)
/// first even if closed.
async fn selectable_revisions<S: WebState>(
    state: &S,
    viewer: &WebUser,
    selected: Option<i64>,
) -> Result<Vec<RevisionOption>, PageError> {
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    // No limit: `u32::MAX` revisions is as good as none.
    let authored =
        storage::load_revisions_by_author(database, &viewer.user.phid, None, u32::MAX as usize)
            .await?;
    let is_open = |revision: &storage::Revision| !revision.is_closed();
    let mut candidates: Vec<storage::Revision> = Vec::new();
    // A selected revision which isn't open comes first.
    if let Some(selected) = selected
        && !authored
            .iter()
            .any(|revision| revision.id == selected && is_open(revision))
    {
        candidates.extend(
            authored
                .iter()
                .filter(|revision| revision.id == selected)
                .cloned(),
        );
    }
    candidates.extend(authored.into_iter().filter(is_open));
    let visible = storage::visible_revisions(
        database,
        state.config(),
        &as_viewer,
        candidates,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    let mut options = Vec::new();
    for revision in visible {
        let can_edit =
            storage::can_edit(database, state.config(), &as_viewer, &revision, state.now())
                .await
                .map_err(harbormaster::internal)?;
        if !can_edit {
            continue;
        }
        options.push(RevisionOption {
            id: revision.id,
            label: truncate(
                &format!("D{} {}", revision.id, revision.title),
                REVISION_NAME_GLYPHS,
            ),
            selected: Some(revision.id) == selected,
        });
    }
    Ok(options)
}

/// `?revisionID=`, the revision the form selects.
fn selected_revision(query: Option<&str>) -> Option<i64> {
    form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .filter(|(key, _)| key == "revisionID")
        .last()
        .and_then(|(_, value)| value.parse().ok())
        .filter(|id| *id != 0)
}

/// `GET /differential/diff/{id}/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let id = parse_id(&id)?;
    let diff = load_diff(state, &viewer, id).await?;
    if let Some(revision_id) = diff.revision_id.filter(|id| *id != 0) {
        return Ok(redirect(state, &format!("/D{revision_id}?id={}", diff.id)));
    }
    let revisions =
        selectable_revisions(state, &viewer, selected_revision(query.as_deref())).await?;
    let mut changesets = phab_diff::storage::load_changeset_list(state.database(), diff.id)
        .await
        .map_err(harbormaster::internal)?;
    phab_diff::sort_for_display(&mut changesets);
    let files = changesets.iter().map(FileItem::new).collect();
    let (_, rendered) =
        crate::revision::changesets(state, &viewer.user.viewer(), &diff.phid, None).await?;
    let title = format!("Diff {}", diff.id);
    Ok(Page(DiffPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
        id: diff.id,
        revisions,
        files,
        changesets: rendered,
    })
    .into_response())
}

/// The `after` cursor of the list; other parameters are a query PHP
/// builds.
fn cursor(query: Option<&str>) -> Result<Option<i64>, PageError> {
    crate::herald::cursor(query)
}

async fn changeset_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: &str,
    uri: &Uri,
    query: Option<&str>,
) -> Result<Response, PageError> {
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let id = parse_id(id)?;
    let diff = load_diff(state, viewer, id).await?;
    let after = cursor(query)?;
    let mut changesets: Vec<Changeset> =
        phab_diff::storage::load_changeset_list(state.database(), diff.id)
            .await
            .map_err(harbormaster::internal)?
            .into_iter()
            .filter(|changeset| after.is_none_or(|after| changeset.id > after))
            .collect();
    let next = (changesets.len() > PAGE_SIZE).then(|| changesets[PAGE_SIZE - 1].id);
    changesets.truncate(PAGE_SIZE);
    let files = changesets.iter().map(FileItem::new).collect();
    Ok(Page(ChangesetList {
        shell: Shell::for_user(
            state.config(),
            viewer,
            "Differential Changesets",
            current(uri),
        ),
        diff_id: diff.id,
        revision_id: diff.revision_id.filter(|id| *id != 0),
        files,
        next,
    })
    .into_response())
}

/// `GET /differential/diff/{id}/changesets/`: the default query, "All
/// Changesets".
pub(crate) async fn changesets<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    changeset_list(state.as_ref(), &viewer, &id, &uri, query.as_deref()).await
}

/// `GET /differential/diff/{id}/changesets/query/{key}/`: the builtin
/// query; saved queries are PHP's.
pub(crate) async fn changeset_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((id, key)): Path<(String, String)>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    if key != "all" {
        return Err(PageError::Fallback("saved changeset queries"));
    }
    changeset_list(state.as_ref(), &viewer, &id, &uri, query.as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_types_are_named_like_php() {
        assert_eq!(change_name(ChangeType::from_value(2)), "Modified");
        assert_eq!(change_name(ChangeType::from_value(10)), "Contents Modified");
        assert_eq!(change_name(ChangeType::from_value(77)), "Unknown");
    }

    #[test]
    fn revision_names_are_truncated() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
    }

    #[test]
    fn the_selected_revision_is_read() {
        assert_eq!(selected_revision(Some("revisionID=12")), Some(12));
        assert_eq!(selected_revision(Some("revisionID=0")), None);
        assert_eq!(selected_revision(None), None);
    }
}
