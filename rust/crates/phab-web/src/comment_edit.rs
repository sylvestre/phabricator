//! The posts of the forms editing and removing a revision comment
//! (`/transactions/edit/<phid>/` and `/transactions/remove/<phid>/`,
//! `PhabricatorApplicationTransactionCommentEditor`): a new version of the
//! comment, which the transaction then points to, then the redirect to the
//! revision (`AphrontReloadResponse`).
//!
//! Left to PHP, before anything is written (421, replayed with the body):
//! dialogs (Ajax and workflow requests), multi-factor prompts (removals by
//! users with a factor, edits of comments signed with MFA), comments of
//! transactions other than `core:comment`, comments the viewer didn't write
//! (administrators removing them), removed or emptied comments, texts with
//! remarkup effects and edits which would subscribe the viewer: both would
//! make the editor apply support transactions (`getExpandedSupportTransactions()`),
//! with Herald, mail and feed.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use phab_app_differential::edit::{decode_attributes, encode_attributes, is_inert_text};
use phab_app_differential::storage::{Revision, Transaction};
use phab_core::{Phid, PhidType, php};
use phab_db::{edges, rows};

use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{transactions, web_edit};

/// `PhabricatorWebContentSource`.
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

/// `PhabricatorObjectHasSubscriberEdgeType`.
const HAS_SUBSCRIBER: i64 = 21;
/// `PhabricatorObjectHasUnsubscriberEdgeType`.
const HAS_UNSUBSCRIBER: i64 = 23;

/// `isDeleted` of a comment emptied by an edit, and of a removed one.
const EMPTIED: i64 = 1;
const REMOVED: i64 = 2;

fn database_error(error: phab_db::sqlx::Error) -> PageError {
    PageError::from(phab_db::DbError::from(error))
}

/// `applyImplicitCC()` would subscribe the viewer: they are a user who is
/// neither subscribed automatically (the author, the reviewers) nor
/// subscribed or unsubscribed explicitly, and not a bot.
async fn would_subscribe<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: &Revision,
) -> Result<bool, PageError> {
    let actor = &viewer.user.phid;
    if viewer.user.is_system_agent || revision.subject().implicit_subscribers.contains(actor) {
        return Ok(false);
    }
    let found = edges::load_destination_phids(
        state.database(),
        std::slice::from_ref(&revision.phid),
        &[HAS_SUBSCRIBER, HAS_UNSUBSCRIBER],
        Some(std::slice::from_ref(actor)),
    )
    .await?;
    Ok(found.is_empty())
}

/// The checks of `validateEdit()` and `applyMFAChecks()` Rust makes, and
/// those which leave the edit to PHP.
async fn check<S: WebState>(
    state: &S,
    viewer: &WebUser,
    transaction: &Transaction,
    revision: &Revision,
    (text, is_removal): (&str, bool),
) -> Result<(), PageError> {
    if transaction.transaction_type != "core:comment" {
        return Err(PageError::Fallback("comments of other transactions"));
    }
    // CAN_EDIT: the transaction's author.
    if transaction.author_phid != viewer.user.phid {
        return Err(PageError::Fallback("comments of other users"));
    }
    if transaction
        .metadata_value("core.mfa")
        .is_some_and(php::is_truthy)
    {
        return Err(PageError::Fallback("comments signed with MFA"));
    }
    if is_removal
        && phab_auth::user::is_enrolled_in_multi_factor(state.database(), &viewer.user.phid).await?
    {
        return Err(PageError::Fallback("a multi-factor prompt"));
    }
    if !is_inert_text(state.config(), text) {
        return Err(PageError::Fallback("comments with remarkup effects"));
    }
    if would_subscribe(state, viewer, revision).await? {
        return Err(PageError::Fallback("comment edits subscribing the viewer"));
    }
    Ok(())
}

/// `applyEdit()`: the new version of the comment and the transaction
/// pointing to it.
async fn apply<S: WebState>(
    state: &S,
    viewer: &WebUser,
    transaction: &Transaction,
    (text, is_deleted): (&str, i64),
) -> Result<(), PageError> {
    let database = state.database();
    let xactions = database.table("differential", "differential_transaction");
    let row = phab_db::sqlx::query(&format!(
        "SELECT commentVersion, oldValue, newValue, metadata FROM {xactions} WHERE phid = ?"
    ))
    .bind(transaction.phid.as_str())
    .fetch_one(database.pool())
    .await
    .map_err(database_error)?;
    // save() encodes the values again.
    if rows::text(&row, "oldValue")? != "null" || rows::text(&row, "newValue")? != "null" {
        return Err(PageError::Fallback("unusual comment transactions"));
    }
    let metadata = decode_attributes(&rows::text(&row, "metadata")?)
        .ok_or(PageError::Fallback("unusual transaction metadata"))?;
    let version = rows::int(&row, "commentVersion")? + 1;
    let actor = viewer.user.phid.as_str();
    let now = state.now();
    let comment_phid = Phid::generate(PhidType::TRANSACTION_COMMENT, None);
    let mut connection = database.pool().begin().await.map_err(database_error)?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
         commentVersion, content, contentSource, isDeleted, dateCreated, dateModified, \
         revisionPHID, changesetID, isNewFile, lineNumber, lineLength, fixedState, hasReplies, \
         replyToCommentPHID, attributes) VALUES (?, ?, ?, 'public', ?, ?, ?, ?, ?, ?, ?, NULL, \
         NULL, 0, 0, 0, NULL, 0, NULL, '[]')",
        database.table("differential", "differential_transaction_comment")
    ))
    .bind(comment_phid.as_str())
    .bind(transaction.phid.as_str())
    .bind(actor)
    .bind(actor)
    .bind(version)
    .bind(text)
    .bind(WEB_SOURCE)
    .bind(is_deleted)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    phab_db::sqlx::query(&format!(
        "UPDATE {xactions} SET commentVersion = ?, commentPHID = ?, viewPolicy = 'public', \
         editPolicy = ?, metadata = ?, dateModified = ? WHERE phid = ?"
    ))
    .bind(version)
    .bind(comment_phid.as_str())
    .bind(actor)
    .bind(encode_attributes(metadata))
    .bind(now)
    .bind(transaction.phid.as_str())
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    connection.commit().await.map_err(database_error)?;
    Ok(())
}

/// Loads the transaction and checks the post; `text` and `is_deleted` are
/// what the form saves.
async fn post_edit<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: LoginRequest,
    phid: &str,
    removal: bool,
) -> Result<Response, PageError> {
    web_edit::check_form_post(state, viewer, &request)?;
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let (transaction, revision) = transactions::load(state, viewer, phid).await?;
    // A removed comment (or one emptied by an edit) goes to PHP, which
    // refuses removed ones.
    transactions::current_comment(state, viewer, &transaction).await?;
    let (text, is_deleted) = if removal {
        (String::new(), REMOVED)
    } else {
        let text = request.str("text").unwrap_or_default().to_owned();
        let is_deleted = if text.is_empty() { EMPTIED } else { 0 };
        (text, is_deleted)
    };
    check(state, viewer, &transaction, &revision, (&text, removal)).await?;
    apply(state, viewer, &transaction, (&text, is_deleted)).await?;
    Ok(request.redirect(state.config(), &format!("/D{}", revision.id)))
}

/// `POST /transactions/edit/<phid>/`.
pub(crate) async fn edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let request = LoginRequest::new(method, uri, headers, &body);
    post_edit(state.as_ref(), &viewer, request, &phid, false).await
}

/// `POST /transactions/remove/<phid>/`.
pub(crate) async fn remove<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let request = LoginRequest::new(method, uri, headers, &body);
    post_edit(state.as_ref(), &viewer, request, &phid, true).await
}
