//! `/transactions/detail/<phid>/`, `/transactions/history/<phid>/` and
//! the other pages of a transaction's comment
//! (`PhabricatorApplicationTransactionDetailController` and the
//! `...Comment*Controller`s): the change details of a transaction, the
//! versions of its comment, its raw text, its quote for a comment form, and
//! the forms editing and removing it, as pages for the links of mail.
//!
//! PHP's pages open these as dialogs (Ajax requests), which stay with PHP,
//! as do transactions of objects other than revisions, change details of
//! transactions other than summary and test plan edits, removed comments,
//! and the posts editing or removing a comment.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_differential::storage::{self, Revision, Transaction};
use phab_core::Phid;
use phab_remarkup::Mode;
use serde_json::Value;

use crate::error::PageError;
use crate::project_edit::DialogPage;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{harbormaster, remarkup, time};

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

/// The comment of the transaction (`getComment()`), if the viewer can see
/// it. A removed comment goes to PHP, which refuses it.
async fn current_comment<S: WebState>(
    state: &S,
    viewer: &WebUser,
    transaction: &Transaction,
) -> Result<storage::Comment, PageError> {
    let phid = transaction
        .comment_phid
        .as_ref()
        .ok_or(PageError::NotFound)?;
    let version = storage::load_comment_versions(state.database(), &transaction.phid)
        .await?
        .into_iter()
        .find(|version| &version.comment.phid == phid)
        .ok_or(PageError::NotFound)?;
    if version.is_deleted {
        return Err(PageError::Fallback("a removed comment"));
    }
    let visible = storage::visible_subjects(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        &[version.comment.subject()],
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    if visible.first() != Some(&true) {
        return Err(PageError::NotFound);
    }
    Ok(version.comment)
}

#[derive(Template)]
#[template(path = "transaction_raw.html")]
pub(crate) struct RawPage {
    shell: Shell,
    revision_uri: String,
    revision_name: String,
    content: String,
}

/// `GET /transactions/raw/<phid>/` (`...CommentRawController`): the text
/// of the comment.
pub(crate) async fn raw<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<RawPage>, PageError> {
    if is_ajax(query.as_deref()) {
        return Err(PageError::Fallback("a transaction dialog"));
    }
    let (transaction, revision) = load(state.as_ref(), &viewer, &phid).await?;
    let comment = current_comment(state.as_ref(), &viewer, &transaction).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(RawPage {
        shell: Shell::for_user(state.config(), &viewer, "Raw Comment", current),
        revision_uri: format!("/D{}", revision.id),
        revision_name: format!("D{}: {}", revision.id, revision.title),
        content: comment.content,
    }))
}

#[derive(Template)]
#[template(path = "transaction_comment_edit.html")]
pub(crate) struct EditPage {
    shell: Shell,
    action: String,
    cancel: String,
    content: String,
    /// `getIsMFATransaction()`: saving asks for MFA.
    is_mfa: bool,
}

/// `GET /transactions/edit/<phid>/` (`...CommentEditController`): the form
/// editing the comment. Saving it (`PhabricatorApplicationTransactionCommentEditor`,
/// not ported) is PHP's. Revisions have no "Interact" policy, so their
/// conversations are never locked.
pub(crate) async fn edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<EditPage>, PageError> {
    if is_ajax(query.as_deref()) {
        return Err(PageError::Fallback("a transaction dialog"));
    }
    let (transaction, revision) = load(state.as_ref(), &viewer, &phid).await?;
    let comment = current_comment(state.as_ref(), &viewer, &transaction).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(EditPage {
        shell: Shell::for_user(state.config(), &viewer, "Edit Comment", current),
        action: uri.path().to_owned(),
        cancel: format!("/D{}", revision.id),
        content: comment.content,
        is_mfa: transaction
            .metadata_value("core.mfa")
            .is_some_and(phab_core::php::is_truthy),
    }))
}

/// `GET /transactions/remove/<phid>/` (`...CommentRemoveController`): the
/// confirmation of removing the comment, which PHP does.
pub(crate) async fn remove<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<DialogPage>, PageError> {
    if is_ajax(query.as_deref()) {
        return Err(PageError::Fallback("a transaction dialog"));
    }
    let (transaction, revision) = load(state.as_ref(), &viewer, &phid).await?;
    current_comment(state.as_ref(), &viewer, &transaction).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(DialogPage::new(
        Shell::for_user(state.config(), &viewer, "Remove Comment", current),
        "Remove Comment",
        vec![
            "Removing a comment prevents anyone (including you) from reading it. Removing a \
             comment also hides the comment's edit history and prevents it from being edited.",
            "Really remove this comment?",
        ],
        uri.path().to_owned(),
        Some("Remove Comment"),
        format!("/D{}", revision.id),
    )))
}

/// The quoted text of a comment: each line behind `>`, under who wrote
/// it (and where, given `reference`).
fn quote_text(content: &str, author: &str, reference: Option<&str>) -> String {
    let content = content.trim_end_matches(['\r', '\n']);
    let quoted: String = if content.is_empty() {
        ">".to_owned()
    } else {
        content
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with('>') {
                    format!(">{line}")
                } else {
                    format!("> {line}")
                }
            })
            .collect()
    };
    let quote = match reference.filter(|reference| !reference.is_empty()) {
        Some(reference) => format!("In {reference}, @{author} wrote:"),
        None => format!("@{author} wrote:"),
    };
    format!(">>! {quote}\n{quoted}")
}

/// `/transactions/quote/<phid>/` (`...CommentQuoteController`): the
/// comment quoted for a comment form, as `{"quoteText": ...}`.
pub(crate) async fn quote<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, PageError> {
    let (transaction, _) = load(state.as_ref(), &viewer, &phid).await?;
    let comment = current_comment(state.as_ref(), &viewer, &transaction).await?;
    // `hasComment()`.
    if comment.content.trim().is_empty() {
        return Err(PageError::NotFound);
    }
    let author = std::slice::from_ref(&comment.author_phid);
    if !author.iter().all(harbormaster::is_supported) {
        return Err(PageError::Fallback("a comment by an object not ported"));
    }
    let scope = harbormaster::Scope::new(state.as_ref(), &viewer);
    let handles = harbormaster::load_handles(state.as_ref(), &scope, author).await?;
    let name = &harbormaster::handle(&handles, &comment.author_phid).name;
    // `getStr('ref')` reads the query string and the posted form.
    let reference = form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .chain(form_urlencoded::parse(&body))
        .filter(|(key, _)| key == "ref")
        .last()
        .map(|(_, value)| value.into_owned());
    Ok(crate::diffusion::ajax_response(&serde_json::json!({
        "quoteText": quote_text(&comment.content, name, reference.as_deref()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_php() {
        assert_eq!(
            quote_text("hello\n> older\n\nbye\r\n", "alice", None),
            ">>! @alice wrote:\n> hello\n>> older\n> \n> bye"
        );
        assert_eq!(
            quote_text("x", "bob", Some("D12")),
            ">>! In D12, @bob wrote:\n> x"
        );
    }
}
