//! `/differential/changeset/` (`DifferentialChangesetViewController`): a
//! changeset shown alone (`?ref=<id>`), and the Ajax requests of PHP's
//! revision and diff pages, which load each changeset and its "show more"
//! context there ([`crate::changeset_ajax`]).
//!
//! The page is Rust's, with the changeset's published inline comments.
//! Left to PHP (`421`): comparisons between two changesets
//! (`?ref=<id>/<vs>`), ranges (`?range=`), view state changes
//! (`?highlight=`, `?encoding=`, `?engine=`, `?renderer=`, `?hidden=`),
//! and the raw files (`?view=old|new`), which PHP stores as files the
//! first time; a binary file's view goes to its file page, as PHP's does.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_app_differential::diff_search::VisibleDiff;
use phab_app_differential::storage::{self, Revision};
use phab_core::Phid;
use phab_diff::{ChangeType, Changeset, FileType};
use phab_policy::application;
use serde_json::Value;

use crate::changeset_ajax::{self, ParseCache, Rendering};
use crate::error::PageError;
use crate::harbormaster::internal;
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// The view state parameters, which PHP saves for the viewer.
const VIEW_STATE_PARAMETERS: &[&str] = &[
    "highlight",
    "encoding",
    "engine",
    "renderer",
    "hidden",
    "discard",
];

#[derive(Template)]
#[template(path = "changeset.html")]
pub(crate) struct ChangesetPage {
    shell: Shell,
    revision_id: Option<i64>,
    diff_id: i64,
    path: String,
    changeset: String,
}

/// `ref`: a changeset ID, or `<id>/<vs>`.
fn parse_reference(reference: &str) -> Result<i64, PageError> {
    let parts: Vec<&str> = reference.split('/').collect();
    let (id, vs) = match parts.as_slice() {
        [id, vs] => (*id, Some(*vs)),
        [id, ..] => (*id, None),
        [] => ("", None),
    };
    if vs.is_some_and(|vs| vs != "0") {
        return Err(PageError::Fallback("comparisons between changesets"));
    }
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageError::Fallback("unusual changeset references"));
    }
    id.parse()
        .map_err(|_| PageError::Fallback("unusual changeset references"))
}

/// The changeset `id` and its diff, if the viewer sees the diff
/// (`DifferentialChangesetQuery` filters the others out).
async fn load<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<(Changeset, VisibleDiff), PageError> {
    let changeset = phab_diff::storage::load_changesets_by_id(state.database(), &[id])
        .await
        .map_err(|error| match error {
            phab_diff::storage::StorageError::Database(error) => internal(error),
            _ => PageError::Fallback("hunks Rust can't read"),
        })?
        .pop()
        .ok_or(PageError::NotFound)?;
    let diff = match crate::diff_page::load_diff(state, viewer, changeset.diff_id).await {
        Ok(diff) => diff,
        Err(PageError::PolicyDenied) => return Err(PageError::NotFound),
        Err(error) => return Err(error),
    };
    Ok((changeset, diff))
}

/// The diff's revision, if it has one the viewer sees.
async fn revision<S: WebState>(
    state: &S,
    viewer: &WebUser,
    diff: &VisibleDiff,
) -> Result<Option<Revision>, PageError> {
    let Some(id) = diff.revision_id.filter(|id| *id != 0) else {
        return Ok(None);
    };
    let found = storage::load_revisions_by_id(state.database(), &[id]).await?;
    Ok(storage::visible_revisions(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        found,
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop())
}

/// `GET|POST /differential/changeset/`.
pub(crate) async fn view<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let request = LoginRequest::new(method, uri.clone(), headers.clone(), &body);
    let is_ajax = request.exists("__ajax__") && !request.exists("__quicksand__");
    let Some(reference) = request.str("ref") else {
        return Err(if is_ajax {
            PageError::Fallback("Ajax requests without a changeset")
        } else {
            PageError::NotFound
        });
    };
    let id = parse_reference(reference)?;
    let (changeset, diff) = match load(state, &viewer, id).await {
        Err(PageError::NotFound) if is_ajax => {
            return Err(PageError::Fallback(
                "Ajax requests for changesets not found",
            ));
        }
        found => found?,
    };
    if let Some(view) = request.nonempty_str("view") {
        let binary = changeset
            .metadata
            .get(format!("{view}:binary-phid"))
            .and_then(Value::as_str)
            .filter(|phid| !phid.is_empty());
        return match binary {
            Some(phid) => Ok(redirect(state, &format!("/file/info/{phid}/"))),
            None => Err(PageError::Fallback("raw files, which PHP may store")),
        };
    }
    if VIEW_STATE_PARAMETERS
        .iter()
        .any(|name| request.str(name).is_some())
    {
        return Err(PageError::Fallback("view state changes"));
    }
    if is_ajax {
        let reference = reference.to_owned();
        return ajax(
            state,
            &viewer,
            (&request, &headers),
            (changeset, diff),
            reference,
        )
        .await;
    }
    if request.nonempty_str("range").is_some() {
        return Err(PageError::Fallback("ranges of a changeset"));
    }
    page(state, &viewer, changeset, diff, &uri).await
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

/// The standalone page.
async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    changeset: Changeset,
    diff: VisibleDiff,
    uri: &Uri,
) -> Result<Response, PageError> {
    let as_viewer = viewer.user.viewer();
    let mut inlines = crate::revision::inlines(state, &as_viewer, &[changeset.id]).await?;
    let inlines = inlines.remove(&changeset.id).unwrap_or_default();
    let options = phab_diff::view::ViewOptions {
        diff_command: crate::revision::diff_command(),
        ..phab_diff::view::ViewOptions::default()
    };
    let path = changeset.display_filename();
    let rendered =
        tokio::task::spawn_blocking(move || match phab_diff::view::build(&changeset, &options) {
            Ok(view) => crate::diff_html::changeset_with_inlines(&view, &inlines, None),
            Err(error) => {
                tracing::warn!(%error, changeset = changeset.id, "changeset failed to render");
                "<section class=\"changeset\"><p class=\"changeset-shield\">This file could not \
                 be shown.</p></section>"
                    .to_owned()
            }
        })
        .await
        .map_err(internal)?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(ChangesetPage {
        shell: Shell::for_user(state.config(), viewer, "Changeset View", current),
        revision_id: diff.revision_id.filter(|id| *id != 0),
        diff_id: diff.id,
        path,
        changeset: rendered,
    })
    .into_response())
}

/// `PhabricatorHash::digestForIndex()` of a changeset's path, which keys
/// its view state.
fn path_hash(changeset: &Changeset) -> String {
    phab_core::hash::digest_for_index(changeset.filename.as_bytes())
}

/// What the viewer's stored view state says about the changeset: the
/// renderer they picked and whether they hid it
/// (`PhabricatorChangesetViewStateEngine`, read-only).
#[derive(Debug, Default, PartialEq, Eq)]
struct ViewState {
    renderer: Option<String>,
    is_hidden: bool,
}

/// The entries of a property, by epoch (`isort()`), last one last.
fn property_entries<'a>(properties: &'a Value, key: &str) -> Vec<&'a Value> {
    let mut entries: Vec<&Value> = match properties.get(key) {
        Some(Value::Object(map)) => map.values().collect(),
        Some(Value::Array(list)) => list.iter().collect(),
        _ => Vec::new(),
    };
    let epoch = |entry: &Value| entry.get("epoch").and_then(Value::as_i64).unwrap_or(0);
    entries.sort_by_key(|entry| epoch(entry));
    entries
}

async fn view_state<S: WebState>(
    state: &S,
    viewer: &WebUser,
    container: &Phid,
    changeset: &Changeset,
) -> Result<ViewState, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT viewState FROM {} WHERE viewerPHID = ? AND objectPHID = ?",
        database.table("differential", "differential_viewstate")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(viewer.user.phid.as_str())
        .bind(container.as_str())
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
    else {
        return Ok(ViewState::default());
    };
    let stored: Value = phab_db::rows::json(&row, "viewState")?;
    let properties = stored
        .get("changesets")
        .and_then(|changesets| changesets.get(path_hash(changeset)))
        .cloned()
        .unwrap_or(Value::Null);
    let value = |key: &str| {
        property_entries(&properties, key)
            .last()
            .and_then(|entry| entry.get("value"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    for key in ["highlight", "encoding", "engine"] {
        if !value(key).is_null() {
            return Err(PageError::Fallback("stored view state Rust doesn't render"));
        }
    }
    let renderer = match value("renderer") {
        Value::Null => None,
        Value::String(renderer) => Some(renderer),
        _ => return Err(PageError::Fallback("unusual view states")),
    };
    // `updateHiddenState()`: hidden for this diff and later ones.
    let is_hidden = property_entries(&properties, "hidden")
        .last()
        .is_some_and(|entry| {
            let value = entry.get("value").is_some_and(phab_core::php::is_truthy);
            let version = entry.get("diffID").and_then(Value::as_i64).unwrap_or(0);
            value && version >= changeset.diff_id
        });
    Ok(ViewState {
        renderer,
        is_hidden,
    })
}

/// Whether the viewer picked unified diffs for every display
/// (`PhabricatorUnifiedDiffsSetting`).
async fn prefers_unified<S: WebState>(state: &S, viewer: &WebUser) -> Result<bool, PageError> {
    let database = state.database();
    let preferences = phab_app_settings::preferences::load(database, &viewer.user.phid)
        .await
        .map_err(internal)?;
    let mut value = preferences.get("diff-unified").clone();
    if value.is_null() {
        value = phab_app_settings::preferences::load_global(database)
            .await
            .map_err(internal)?
            .and_then(|global| global.get("diff-unified").cloned())
            .unwrap_or(Value::Null);
    }
    Ok(value == Value::String("unified".to_owned()))
}

/// The build targets of the diff's buildables, whose lint messages and
/// coverage PHP shows on the changeset (`getBuildTargetPHIDs()`).
async fn has_lint_or_coverage<S: WebState>(
    state: &S,
    diff: &VisibleDiff,
    changeset: &Changeset,
) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT t.phid FROM {} t JOIN {} b ON b.phid = t.buildPHID JOIN {} a ON a.phid = \
         b.buildablePHID WHERE a.buildablePHID = ? AND a.isManualBuildable = 0",
        database.table("harbormaster", "harbormaster_buildtarget"),
        database.table("harbormaster", "harbormaster_build"),
        database.table("harbormaster", "harbormaster_buildable"),
    );
    let targets: Vec<String> = phab_db::sqlx::query(&sql)
        .bind(diff.phid.as_str())
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .iter()
        .map(|row| phab_db::rows::text(row, "phid"))
        .collect::<Result<_, _>>()?;
    if targets.is_empty() {
        return Ok(false);
    }
    let lint = format!(
        "SELECT 1 FROM {} WHERE buildTargetPHID IN ({}) AND path = ? LIMIT 1",
        database.table("harbormaster", "harbormaster_buildlintmessage"),
        phab_db::placeholders(targets.len()),
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&lint),
        targets.iter().map(String::as_str),
    )
    .bind(changeset.filename.as_str())
    .fetch_optional(database.pool())
    .await
    .map_err(phab_db::DbError::from)?;
    if found.is_some() {
        return Ok(true);
    }
    let unit = format!(
        "SELECT properties FROM {} WHERE buildTargetPHID IN ({})",
        database.table("harbormaster", "harbormaster_buildunitmessage"),
        phab_db::placeholders(targets.len()),
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&unit),
        targets.iter().map(String::as_str),
    )
    .fetch_all(database.pool())
    .await
    .map_err(phab_db::DbError::from)?;
    for row in &rows {
        let properties: Value = phab_db::rows::json(row, "properties")?;
        match properties.get("coverage") {
            None | Some(Value::Null) => {}
            Some(coverage) => {
                // Coverage of this file, or something PHP may read as such.
                if !coverage.is_object()
                    || coverage
                        .get(&changeset.filename)
                        .is_some_and(|data| !matches!(data, Value::String(text) if text.is_empty()))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Whether inline comments may show on the changeset: on it, or on a
/// changeset of the revision with its path, which PHP ports to it
/// (`PhabricatorInlineCommentAdjustmentEngine`).
async fn has_inlines<S: WebState>(
    state: &S,
    revision: &Revision,
    changeset: &Changeset,
) -> Result<bool, PageError> {
    let database = state.database();
    let mut paths = vec![changeset.filename.as_str()];
    if changeset.change_type == ChangeType::MoveHere
        && let Some(old) = changeset.old_file.as_deref()
    {
        paths.push(old);
    }
    let sql = format!(
        "SELECT 1 FROM {} c LEFT JOIN {} s ON s.id = c.changesetID WHERE c.revisionPHID = ? \
         AND c.changesetID IS NOT NULL AND (c.changesetID = ? OR s.filename IN ({})) LIMIT 1",
        database.table("differential", "differential_transaction_comment"),
        database.table("differential", "differential_changeset"),
        phab_db::placeholders(paths.len()),
    );
    let query = phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(changeset.id);
    Ok(phab_db::bind_all(query, paths.iter().copied())
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// The stored parse of the changeset, if PHP rendered it already.
async fn parse_cache<S: WebState>(
    state: &S,
    changeset: &Changeset,
) -> Result<Option<Vec<u8>>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT cache FROM {} WHERE cacheIndex = ?",
        database.table("differential", "differential_changeset_parse_cache")
    );
    let key = phab_core::hash::digest_for_index(changeset.id.to_string().as_bytes());
    let row = phab_db::sqlx::query(&sql)
        .bind(key.as_bytes())
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    row.map(|row| phab_db::sqlx::Row::try_get::<Vec<u8>, _>(&row, "cache"))
        .transpose()
        .map_err(|error| internal(phab_db::DbError::from(error)))
}

/// The data encoding of the first hunk (`getDataEncoding()`).
async fn first_hunk_encoding<S: WebState>(
    state: &S,
    changeset: &Changeset,
) -> Result<Option<String>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT dataEncoding FROM {} WHERE changesetID = ? ORDER BY id LIMIT 1",
        database.table("differential", "differential_hunk")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(changeset.id)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    Ok(match row {
        Some(row) => phab_db::rows::opt_text(&row, "dataEncoding")?,
        None => None,
    })
}

/// `getChangesetProperties()` differs between the sides: PHP shows a
/// table of the changes, which Rust doesn't render.
fn has_property_changes(changeset: &Changeset) -> bool {
    let only_file_mode = |properties: &Value| match properties {
        Value::Object(map) => map.keys().all(|key| key == "unix:filemode"),
        Value::Array(list) => list.is_empty(),
        Value::Null => true,
        _ => false,
    };
    if !only_file_mode(&changeset.old_properties) || !only_file_mode(&changeset.new_properties) {
        return true;
    }
    let mode = |properties: &Value| properties.get("unix:filemode").cloned();
    let default = Value::String("100644".to_owned());
    let (old, new) = (
        mode(&changeset.old_properties),
        mode(&changeset.new_properties),
    );
    let is_default = |value: &Option<Value>| value.as_ref().unwrap_or(&default) == &default;
    let modes_differ = !(is_default(&old) && is_default(&new)) && old != new;
    let metadata = |key: &str| changeset.metadata.get(key).cloned();
    modes_differ
        || metadata("old:file:mime-type") != metadata("new:file:mime-type")
        || metadata("old:file:size") != metadata("new:file:size")
}

/// The Ajax answer, from PHP's parse cache.
async fn ajax<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (request, request_headers): (&LoginRequest, &HeaderMap),
    (changeset, diff): (Changeset, VisibleDiff),
    reference: String,
) -> Result<Response, PageError> {
    if request.is_post() {
        let token = request.str("__csrf__").unwrap_or_default();
        if !viewer.is_valid_csrf_token(token, state.now())? {
            return Err(PageError::Fallback("invalid CSRF tokens"));
        }
    }
    if changeset.file_type != FileType::Text
        || ["old:binary-phid", "new:binary-phid"].iter().any(|key| {
            changeset
                .metadata
                .get(*key)
                .is_some_and(|phid| !phid.is_null())
        })
    {
        return Err(PageError::Fallback("changesets which aren't text"));
    }
    let revision = revision(state, viewer, &diff).await?;
    let container = revision
        .as_ref()
        .map_or_else(|| diff.phid.clone(), |revision| revision.phid.clone());
    let view_state = view_state(state, viewer, &container, &changeset).await?;
    let renderer = match &view_state.renderer {
        Some(renderer) => Some(renderer.clone()),
        None if prefers_unified(state, viewer).await? => Some("1up".to_owned()),
        None => request.str("device").map(str::to_owned),
    };
    if renderer.as_deref() == Some("1up") {
        return Err(PageError::Fallback("the unified renderer"));
    }
    if has_lint_or_coverage(state, &diff, &changeset).await? {
        return Err(PageError::Fallback(
            "changesets with lint messages or coverage",
        ));
    }
    if let Some(revision) = &revision
        && has_inlines(state, revision, &changeset).await?
    {
        return Err(PageError::Fallback("changesets with inline comments"));
    }
    let range = changeset_ajax::parse_range(request.str("range"));
    let is_top_level = range.0.is_none() && range.1.is_none();
    let data = parse_cache(state, &changeset)
        .await?
        .ok_or(PageError::Fallback("changesets PHP has not rendered yet"))?;
    let cache = ParseCache::decode(&data, is_top_level)?
        .ok_or(PageError::Fallback("parse caches PHP would rebuild"))?;
    if changeset_ajax::may_use_document_engine(&changeset, &cache) {
        return Err(PageError::Fallback(
            "changesets a document engine may render",
        ));
    }
    if is_top_level && has_property_changes(&changeset) {
        return Err(PageError::Fallback("changesets with property changes"));
    }
    let encoding = first_hunk_encoding(state, &changeset).await?;
    let block = request
        .str("__metablock__")
        .map_or(Ok(0), |block| {
            if block.is_empty() {
                Ok(0)
            } else {
                block.parse::<i64>()
            }
        })
        .map_err(|_| PageError::Fallback("unusual metadata blocks"))?;
    let request = changeset_ajax::Request { reference, range };
    let rendering = Rendering {
        changeset: &changeset,
        cache: &cache,
        request: &request,
        encoding: encoding.as_deref(),
        is_hidden: view_state.is_hidden,
    };
    let answer = changeset_ajax::response(&rendering, block)?;
    let body = format!("for (;;);{}", phab_conduit::json::encode(&answer));
    Ok(crate::datasource::ajax_response(
        state,
        request_headers,
        body,
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn references_name_one_changeset() {
        assert_eq!(parse_reference("12").unwrap(), 12);
        assert_eq!(parse_reference("12/0").unwrap(), 12);
        assert!(parse_reference("12/13").is_err());
        assert!(parse_reference("x").is_err());
    }

    #[test]
    fn the_last_entry_by_epoch_wins() {
        let properties = json!({"renderer": {
            "5": {"value": "1up", "epoch": 20},
            "4": {"value": "2up", "epoch": 10},
        }});
        let entries = property_entries(&properties, "renderer");
        assert_eq!(entries.last().unwrap()["value"], "1up");
    }
}
