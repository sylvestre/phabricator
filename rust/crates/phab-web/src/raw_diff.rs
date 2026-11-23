//! "Download Raw Diff" (`/D<id>?download=true`,
//! `DifferentialRevisionViewController::buildRawDiffResponse()`): the
//! shown diff as a patch, stored as a temporary file attached to the
//! revision, and a redirect to the file.
//!
//! The patch is `ArcanistBundle::toGitPatch()` for revisions of Git and
//! Mercurial repositories. Left to PHP (`421`, before writing): diffs
//! without a repository the viewer sees (`toUnifiedDiff()`), comparisons
//! between two diffs, binary changes and multicopies, patches large
//! enough to be stored in chunks, and the files `file.upload` leaves to
//! PHP (other storage engines, encryption).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use phab_app_differential::diff_search::VisibleDiff;
use phab_app_differential::{Revision, repository};
use phab_app_files::storage::{self, Engines, Properties};
use phab_conduit::MethodError;
use phab_core::Phid;
use serde_json::Map;

use crate::error::PageError;
use crate::php_headers::{self, Options};
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// The file is gone after 24 hours (`ttl.relative`), which its name says.
const RELATIVE_TTL: i64 = 24 * 60 * 60;

/// `PhabricatorFileAttachment::MODE_ATTACH`.
const MODE_ATTACH: &str = "attach";

fn method_error(error: MethodError) -> PageError {
    match error {
        MethodError::Unsupported(reason) => PageError::Fallback(reason),
        MethodError::Conduit { .. } => PageError::Fallback("files PHP refuses to store"),
        MethodError::Internal(error) => PageError::Internal(Box::new(std::io::Error::other(error))),
    }
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

/// The version control system of the diff's repository, if the viewer
/// sees it (`$repository`).
async fn repository_vcs<S: WebState>(
    state: &S,
    viewer: &WebUser,
    diff: &VisibleDiff,
) -> Result<Option<String>, PageError> {
    let Some(phid) = &diff.repository_phid else {
        return Ok(None);
    };
    let database = state.database();
    let found = repository::load(database, &[Phid::new(phid.as_str())]).await?;
    let visible = repository::visible(
        database,
        state.config(),
        &viewer.user.viewer(),
        found,
        state.now(),
    )
    .await
    .map_err(internal)?;
    Ok(visible.into_iter().next().map(|repository| repository.vcs))
}

/// The patch of the diff's changesets, in the order the page shows them.
async fn patch<S: WebState>(
    state: &S,
    viewer: &WebUser,
    diff: &VisibleDiff,
) -> Result<String, PageError> {
    match repository_vcs(state, viewer, diff).await?.as_deref() {
        Some("git" | "hg") => {}
        _ => return Err(PageError::Fallback("raw diffs as unified diffs")),
    }
    let mut changesets = phab_diff::storage::load_changesets(state.database(), diff.id)
        .await
        .map_err(|error| match error {
            phab_diff::storage::StorageError::Database(error) => internal(error),
            _ => PageError::Fallback("hunks Rust can't read"),
        })?;
    phab_diff::sort_for_display(&mut changesets);
    phab_diff::git_patch::to_git_patch(&changesets)
        .map_err(|_| PageError::Fallback("raw diffs with binary changes or multicopies"))
}

/// `PhabricatorFile::getRedirectResponse()`: a redirect to the file, with
/// the headers of PHP's `AphrontRedirectResponse`.
fn redirect<S: WebState>(state: &S, request: &HeaderMap, location: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    let headers = response.headers_mut();
    php_headers::base_headers(headers, state.config(), request);
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, value);
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=UTF-8"),
    );
    php_headers::cache_headers(headers, &Options::default(), state.now());
    response
}

/// Stores the patch of `diff` as PHP does and redirects to it; `path` is
/// the request's path, which names the file.
pub(crate) async fn download<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (revision, diff): (&Revision, &VisibleDiff),
    path: &str,
    request_headers: &HeaderMap,
) -> Result<Response, PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let text = patch(state, viewer, diff).await?;
    let config = state.config();
    let now = state.now();
    let length = i64::try_from(text.len()).map_err(internal)?;
    let engines = Engines::from_config(config).map_err(method_error)?;
    if engines
        .chunk_threshold()
        .is_some_and(|threshold| length > threshold)
    {
        return Err(PageError::Fallback("raw diffs stored in chunks"));
    }
    // "D123.1692295858.diff": the path, then when the file expires.
    let name = format!(
        "{}.{}.diff",
        path.trim_start_matches('/'),
        now + RELATIVE_TTL
    );
    let properties = Properties {
        name: Some(&name),
        ttl: Some(now + RELATIVE_TTL),
        view_policy: Some("no-one"),
        mime_type: Some("text/plain"),
        ..Properties::default()
    };
    let context = web_edit::context(state);
    let request = web_edit::request(state, &context, viewer, Map::new());
    let prepared = storage::from_data(&request, text.as_bytes(), &properties, None)
        .await
        .map_err(method_error)?;

    let database = state.database();
    let mut connection = database
        .pool()
        .acquire()
        .await
        .map_err(phab_db::DbError::from)?;
    let phid = prepared
        .write(&mut connection, database, now)
        .await
        .map_err(method_error)?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (objectPHID, filePHID, attachmentMode, attacherPHID, dateCreated, \
         dateModified) VALUES (?, ?, ?, NULL, ?, ?) ON DUPLICATE KEY UPDATE attachmentMode = \
         VALUES(attachmentMode), attacherPHID = VALUES(attacherPHID), dateModified = \
         VALUES(dateModified)",
        database.table("file", "file_attachment")
    ))
    .bind(revision.phid.as_str())
    .bind(phid.as_str())
    .bind(MODE_ATTACH)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(phab_db::DbError::from)?;

    let file = phab_app_files::load_by_phids(database, std::slice::from_ref(&phid))
        .await?
        .pop()
        .ok_or_else(|| internal(std::io::Error::other("the new file did not load")))?;
    let uri = file.best_uri(config);
    // A local URI (the file's page) is made absolute.
    let location = if uri.starts_with('/') {
        config.uri(&uri).unwrap_or(uri)
    } else {
        uri
    };
    Ok(redirect(state, request_headers, &location))
}
