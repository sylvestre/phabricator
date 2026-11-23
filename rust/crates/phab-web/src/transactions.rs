//! `/transactions/detail/<phid>/` and `/transactions/history/<phid>/`
//! (`PhabricatorApplicationTransactionDetailController` and
//! `...CommentHistoryController`): the change details of a transaction and
//! the versions of its comment, as pages for the links of mail.
//!
//! PHP's pages open these as dialogs (Ajax requests), which stay with PHP,
//! as do transactions of objects other than revisions, change details of
//! transactions other than summary and test plan edits, and removed
//! comments.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_differential::storage::{self, Revision, Transaction};
use phab_core::Phid;
use phab_remarkup::Mode;
use serde_json::Value;

use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{remarkup, time};

/// `PhabricatorDifferentialApplication`.
const DIFFERENTIAL: &str = "PhabricatorDifferentialApplication";

#[derive(Template)]
#[template(path = "transaction_detail.html")]
pub(crate) struct DetailPage {
    shell: Shell,
    revision_uri: String,
    revision_name: String,
    old_text: String,
    new_text: String,
}

/// A version of the comment.
pub(crate) struct Version {
    number: i64,
    date: String,
    html: String,
}

#[derive(Template)]
#[template(path = "transaction_history.html")]
pub(crate) struct HistoryPage {
    shell: Shell,
    revision_uri: String,
    revision_name: String,
    versions: Vec<Version>,
}

/// A request from a PHP page's dialog workflow.
fn is_ajax(query: Option<&str>) -> bool {
    form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .any(|(key, _)| key == "__ajax__" || key == "__dialog__")
}

/// The revision transaction with this PHID and its revision, as the
/// viewer sees them (`PhabricatorObjectQuery`).
async fn load<S: WebState>(
    state: &S,
    viewer: &WebUser,
    phid: &str,
) -> Result<(Transaction, Revision), PageError> {
    if !phid.starts_with("PHID-XACT-DREV-") {
        return Err(PageError::Fallback("a transaction of an object not ported"));
    }
    viewer.require_application(state, DIFFERENTIAL).await?;
    let database = state.database();
    let internal = |error: storage::VisibilityError| PageError::Internal(Box::new(error));
    let (transaction, _) = storage::load_transaction(database, &Phid::new(phid))
        .await?
        .ok_or(PageError::NotFound)?;
    let as_viewer = viewer.user.viewer();
    let revision = storage::visible_revisions(
        database,
        state.config(),
        &as_viewer,
        storage::load_revisions(database, std::slice::from_ref(&transaction.object_phid)).await?,
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop()
    .ok_or(PageError::NotFound)?;
    let visible = storage::visible_subjects(
        database,
        state.config(),
        &as_viewer,
        &[transaction.subject()],
        state.now(),
    )
    .await
    .map_err(internal)?;
    if visible.first() != Some(&true) {
        return Err(PageError::NotFound);
    }
    Ok((transaction, revision))
}

/// `GET /transactions/detail/<phid>/`.
pub(crate) async fn detail<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<DetailPage>, PageError> {
    if is_ajax(query.as_deref()) {
        return Err(PageError::Fallback("a transaction dialog"));
    }
    let (transaction, revision) = load(state.as_ref(), &viewer, &phid).await?;
    if !matches!(
        transaction.transaction_type.as_str(),
        "differential.revision.summary" | "differential.revision.testplan"
    ) {
        return Err(PageError::Fallback("change details not ported"));
    }
    let text = |value: &Value| -> Result<String, PageError> {
        match value {
            Value::Null => Ok(String::new()),
            Value::String(text) => Ok(text.clone()),
            _ => Err(PageError::Fallback("change details which are not text")),
        }
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(DetailPage {
        shell: Shell::for_user(state.config(), &viewer, "Change Details", current),
        revision_uri: format!("/D{}", revision.id),
        revision_name: format!("D{}: {}", revision.id, revision.title),
        old_text: text(&transaction.old_value)?,
        new_text: text(&transaction.new_value)?,
    }))
}

/// `GET /transactions/history/<phid>/`.
pub(crate) async fn history<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<HistoryPage>, PageError> {
    if is_ajax(query.as_deref()) {
        return Err(PageError::Fallback("a transaction dialog"));
    }
    let (transaction, revision) = load(state.as_ref(), &viewer, &phid).await?;
    if transaction.comment_phid.is_none() {
        return Err(PageError::NotFound);
    }
    let database = state.database();
    let versions = storage::load_comment_versions(database, &Phid::new(phid.as_str())).await?;
    let current_comment = versions
        .iter()
        .find(|version| Some(&version.comment.phid) == transaction.comment_phid.as_ref());
    if current_comment.is_none_or(|version| version.is_deleted) {
        return Err(PageError::Fallback("a removed comment"));
    }
    let as_viewer = viewer.user.viewer();
    let subjects: Vec<_> = versions
        .iter()
        .map(|version| version.comment.subject())
        .collect();
    let visible =
        storage::visible_subjects(database, state.config(), &as_viewer, &subjects, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    let shown: Vec<_> = versions
        .iter()
        .zip(visible)
        .filter_map(|(version, visible)| visible.then_some(version))
        .collect();
    if shown.is_empty() {
        return Err(PageError::NotFound);
    }
    if shown.iter().any(|version| version.is_deleted) {
        return Err(PageError::Fallback("a removed comment version"));
    }
    let texts: Vec<&str> = shown
        .iter()
        .map(|version| version.comment.content.as_str())
        .collect();
    let rendered = remarkup::render_all(state.as_ref(), &as_viewer, &texts, Mode::Html).await?;
    let versions = shown
        .iter()
        .zip(rendered)
        .map(|(version, markup)| Version {
            number: version.version,
            date: time::format_utc(version.date_created),
            html: String::from_utf8_lossy(markup.as_bytes()).into_owned(),
        })
        .collect();
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(HistoryPage {
        shell: Shell::for_user(state.config(), &viewer, "Comment History", current),
        revision_uri: format!("/D{}", revision.id),
        revision_name: format!("D{}: {}", revision.id, revision.title),
        versions,
    }))
}
