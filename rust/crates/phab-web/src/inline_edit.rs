//! `POST /differential/comment/inline/edit/<id>/`
//! (`DifferentialInlineCommentEditController` and
//! `PhabricatorInlineCommentController`): the Ajax operations of the inline
//! comment editor of PHP's revision pages, which change a draft inline
//! comment without rendering it.
//!
//! Ported: hiding and showing comments (`hide`, `show`), marking one done
//! (`done`), deleting and restoring a draft (`delete`, `undelete`, and
//! `refdelete` once confirmed), closing the editor (`cancel`) and saving
//! the editor's text as a versioned draft (`draft`). Each writes the rows
//! PHP's does: the comment's row as `DifferentialInlineComment::save()`
//! saves it, the purge of its versioned drafts, and the "has draft" edge of
//! the revision as `DifferentialRevisionDraftEngine` synchronizes it.
//!
//! Left to PHP, before anything is written (421, replayed with the body):
//! the operations which answer with the editor's or the comment's HTML
//! (`new`, `reply`, `edit`, `save`), the confirmation dialog of
//! `refdelete`, replies (`replyToCommentPHID`), requests PHP refuses (a bad
//! CSRF token, a 404; comments or revisions which don't load or which the
//! viewer may not edit, exceptions), values Rust doesn't read as PHP
//! (attributes, draft properties, IDs which are not integers), and comments
//! whose context PHP would compute and store in its cache
//! (`needInlineContext()`: a changeset with a single hunk at the top of the
//! file, not cached yet).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use phab_app_differential::edit::{decode_attributes, encode_attributes};
use phab_app_differential::storage::{self, Revision};
use phab_core::{Phid, php};
use phab_db::rows;
use phab_policy::application;
use serde_json::{Map, Value, json};

use crate::error::PageError;
use crate::harbormaster::{self, Scope, internal};
use crate::login::request::LoginRequest;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorOldWorldContentSource`, which `getTransactionCommentForSave()`
/// gives the comment.
const LEGACY_SOURCE: &str = r#"{"source":"legacy","params":[]}"#;

/// `PhabricatorObjectHasDraftEdgeType`.
const OBJECT_HAS_DRAFT: i64 = 64;

/// `PhabricatorDiffInlineCommentQuery::INLINE_CONTEXT_CACHE_VERSION`.
const CONTEXT_CACHE_VERSION: i64 = 1;

fn database_error(error: phab_db::sqlx::Error) -> PageError {
    PageError::from(phab_db::DbError::from(error))
}

/// A `differential_transaction_comment` row of an inline comment
/// (`DifferentialDiffInlineCommentQuery`: one with a changeset).
struct Inline {
    id: i64,
    phid: Phid,
    author_phid: Phid,
    view_policy: String,
    edit_policy: String,
    revision_phid: Option<Phid>,
    transaction_phid: Option<Phid>,
    changeset_id: i64,
    fixed_state: Option<String>,
    content: String,
    is_deleted: bool,
    attributes: Map<String, Value>,
}

impl Inline {
    fn subject(&self) -> phab_policy::PolicySubject {
        storage::Comment {
            phid: self.phid.clone(),
            author_phid: self.author_phid.clone(),
            view_policy: self.view_policy.clone(),
            edit_policy: self.edit_policy.clone(),
            content: String::new(),
        }
        .subject()
    }

    /// `isDraft()`.
    fn is_draft(&self) -> bool {
        self.transaction_phid.is_none()
    }
}

/// The inline comment with this ID, if the viewer can see it
/// (`loadCommentByID()`); PHP fails on the others.
async fn load_inline<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<Inline, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT id, phid, authorPHID, viewPolicy, editPolicy, revisionPHID, transactionPHID, \
           changesetID, fixedState, content, isDeleted, attributes FROM {} \
         WHERE id = ? AND changesetID IS NOT NULL",
        database.table("differential", "differential_transaction_comment")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await
        .map_err(database_error)?
        .ok_or(PageError::Fallback("inline comments which don't load"))?;
    let attributes = decode_attributes(&rows::text(&row, "attributes")?)
        .ok_or(PageError::Fallback("unusual inline comment attributes"))?;
    let inline = Inline {
        id: rows::int(&row, "id")?,
        phid: Phid::new(rows::text(&row, "phid")?),
        author_phid: Phid::new(rows::text(&row, "authorPHID")?),
        view_policy: rows::text(&row, "viewPolicy")?,
        edit_policy: rows::text(&row, "editPolicy")?,
        revision_phid: rows::opt_phid(&row, "revisionPHID")?,
        transaction_phid: rows::opt_phid(&row, "transactionPHID")?,
        changeset_id: rows::int(&row, "changesetID")?,
        fixed_state: rows::opt_text(&row, "fixedState")?,
        content: rows::opt_text(&row, "content")?.unwrap_or_default(),
        is_deleted: rows::flag(&row, "isDeleted")?,
        attributes,
    };
    let visible = storage::visible_subjects(
        database,
        state.config(),
        &viewer.user.viewer(),
        &[inline.subject()],
        state.now(),
    )
    .await
    .map_err(internal)?;
    if visible.first() != Some(&true) {
        return Err(PageError::Fallback("inline comments which don't load"));
    }
    Ok(inline)
}

/// The revision with this ID, if the viewer can see it; PHP fails on the
/// others.
async fn visible_revision<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<Revision, PageError> {
    let database = state.database();
    let found = storage::load_revisions_by_id(database, &[id]).await?;
    storage::visible_revisions(
        database,
        state.config(),
        &viewer.user.viewer(),
        found,
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop()
    .ok_or(PageError::Fallback("revisions which don't load"))
}

/// `loadCommentForDone()`: the comment, which the revision's author may
/// mark done, as may the author of a draft.
async fn load_for_done<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<Inline, PageError> {
    let inline = load_inline(state, viewer, id).await?;
    let changeset = storage::load_changeset_diff_ids(state.database(), &[inline.changeset_id])
        .await?
        .remove(&inline.changeset_id)
        .ok_or(PageError::Fallback("changesets which don't load"))?;
    let diff = match crate::diff_page::load_diff(state, viewer, changeset).await {
        Ok(diff) => diff,
        Err(PageError::NotFound | PageError::PolicyDenied) => {
            return Err(PageError::Fallback("diffs which don't load"));
        }
        Err(error) => return Err(error),
    };
    let revision_id = diff
        .revision_id
        .ok_or(PageError::Fallback("revisions which don't load"))?;
    let revision = visible_revision(state, viewer, revision_id).await?;
    let viewer_phid = &viewer.user.phid;
    let is_owner = revision.author_phid.as_ref() == Some(viewer_phid);
    let is_author = &inline.author_phid == viewer_phid;
    if !(is_owner || (is_author && inline.is_draft())) {
        return Err(PageError::Fallback(
            "inline comments the viewer may not mark done",
        ));
    }
    Ok(inline)
}

/// `loadCommentByIDForEdit()`: the viewer's draft on the revision of the
/// URI. Its context (`needInlineContext()`) is read from PHP's cache;
/// computing it would store it there, which Rust leaves to PHP.
async fn load_for_edit<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
    revision: &Revision,
) -> Result<Inline, PageError> {
    let inline = load_inline(state, viewer, id).await?;
    // canEditInlineComment().
    if inline.author_phid != viewer.user.phid
        || !inline.is_draft()
        || inline.revision_phid.as_ref() != Some(&revision.phid)
    {
        return Err(PageError::Fallback(
            "inline comments the viewer may not edit",
        ));
    }
    let has_engine = !matches!(
        inline.attributes.get("documentEngineKey"),
        None | Some(Value::Null)
    );
    if has_engine {
        return Ok(inline);
    }
    let key = format!("inline({}).context(v{CONTEXT_CACHE_VERSION})", inline.phid);
    let scope = Scope::new(state, viewer);
    let cached = phab_app_diffusion::cache::read(&scope.request(), std::slice::from_ref(&key))
        .await
        .map_err(harbormaster::method_error)?;
    if cached.first().is_some_and(Option::is_some) {
        return Ok(inline);
    }
    let changeset =
        phab_diff::storage::load_changesets_by_id(state.database(), &[inline.changeset_id])
            .await
            .map_err(|error| match error {
                phab_diff::storage::StorageError::Database(error) => internal(error),
                _ => PageError::Fallback("hunks Rust can't read"),
            })?
            .pop();
    // newInlineContextMap() only builds (and caches) the context of a
    // changeset with one hunk starting on the first line.
    if let Some(changeset) = changeset {
        let simple = changeset.hunks.len() == 1
            && changeset.hunks[0].old_offset <= 1
            && changeset.hunks[0].new_offset <= 1;
        if simple {
            return Err(PageError::Fallback(
                "inline comments whose context PHP caches",
            ));
        }
    }
    Ok(inline)
}

/// `(string)` of a stored value, for the values Rust casts as PHP.
fn php_string(value: Option<&Value>) -> Result<String, PageError> {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Ok(String::new()),
        Some(Value::Bool(true)) => Ok("1".to_owned()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => Ok(number.to_string()),
        Some(_) => Err(PageError::Fallback("unusual inline comment states")),
    }
}

/// A content state (`PhabricatorDiffInlineCommentContentState`): the text,
/// whether it suggests an edit, and the suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentState {
    text: String,
    has_suggestion: bool,
    suggestion: String,
}

impl ContentState {
    /// `readStorageMap()`.
    fn read(map: &Map<String, Value>) -> Result<Self, PageError> {
        Ok(Self {
            text: php_string(map.get("text"))?,
            has_suggestion: map.get("hasSuggestion").is_some_and(php::is_truthy),
            suggestion: php_string(map.get("suggestionText"))?,
        })
    }

    /// `newStorageMapFromRequest()`.
    fn from_request(request: &LoginRequest) -> Self {
        Self {
            text: request.str("text").unwrap_or_default().to_owned(),
            has_suggestion: request.bool("hasSuggestion"),
            suggestion: request.str("suggestionText").unwrap_or_default().to_owned(),
        }
    }

    /// `newStorageMap()`.
    fn storage_map(&self) -> Value {
        json!({
            "text": self.text,
            "hasSuggestion": self.has_suggestion,
            "suggestionText": self.suggestion,
        })
    }

    /// `isEmptyContentState()`.
    fn is_empty(&self) -> bool {
        self.text.is_empty() && (!self.has_suggestion || self.suggestion.is_empty())
    }
}

/// The properties of a versioned draft, decoded.
fn draft_properties(text: &str) -> Result<Map<String, Value>, PageError> {
    decode_attributes(text).ok_or(PageError::Fallback("unusual versioned draft properties"))
}

/// The properties of the newest versioned draft of `object` by `author`
/// (`PhabricatorVersionedDraft::loadDraft()`), with its ID.
async fn newest_draft<S: WebState>(
    state: &S,
    object: &str,
    author: &str,
) -> Result<Option<(i64, Map<String, Value>)>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT id, properties FROM {} WHERE objectPHID = ? AND authorPHID = ? \
         ORDER BY version DESC LIMIT 1",
        database.table("draft", "draft_versioneddraft")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(object)
        .bind(author)
        .fetch_optional(database.pool())
        .await
        .map_err(database_error)?
    else {
        return Ok(None);
    };
    Ok(Some((
        rows::int(&row, "id")?,
        draft_properties(&rows::text(&row, "properties")?)?,
    )))
}

/// `PhabricatorDraftEngine::synchronize()` of the revision for the viewer:
/// the "has draft" edge, when the viewer's versioned draft of the revision
/// has a comment or actions, or one of their drafts on it isn't void.
async fn synchronize<S: WebState>(
    state: &S,
    revision: &Revision,
    viewer: &Phid,
) -> Result<(), PageError> {
    let has_draft = has_draft_content(state, revision, viewer).await?;
    let database = state.database();
    let table = database.table("differential", "edge");
    let query = if has_draft {
        // The edge type has no inverse and no data.
        format!(
            "INSERT INTO {table} (src, type, dst, dateCreated, seq, dataID) \
             VALUES (?, ?, ?, ?, 0, NULL) ON DUPLICATE KEY UPDATE dataID = VALUES(dataID)"
        )
    } else {
        format!("DELETE FROM {table} WHERE src = ? AND type = ? AND dst = ?")
    };
    let mut query = phab_db::sqlx::query(&query)
        .bind(revision.phid.as_str())
        .bind(OBJECT_HAS_DRAFT)
        .bind(viewer.as_str());
    if has_draft {
        query = query.bind(state.now());
    }
    query
        .execute(database.pool())
        .await
        .map_err(database_error)?;
    Ok(())
}

/// `hasAnyDraftContent()` of `DifferentialRevisionDraftEngine`.
async fn has_draft_content<S: WebState>(
    state: &S,
    revision: &Revision,
    viewer: &Phid,
) -> Result<bool, PageError> {
    if let Some((_, properties)) =
        newest_draft(state, revision.phid.as_str(), viewer.as_str()).await?
        && ["comment", "actions"]
            .iter()
            .any(|key| properties.get(*key).is_some_and(php::is_truthy))
    {
        return Ok(true);
    }
    // withPublishableComments(): the viewer's drafts which aren't void,
    // with the text of their versioned draft while they are edited.
    let database = state.database();
    let sql = format!(
        "SELECT phid, content, attributes FROM {} WHERE changesetID IS NOT NULL AND \
         revisionPHID = ? AND authorPHID = ? AND isDeleted = 0 AND transactionPHID IS NULL \
         ORDER BY id DESC",
        database.table("differential", "differential_transaction_comment")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(viewer.as_str())
        .fetch_all(database.pool())
        .await
        .map_err(database_error)?;
    for row in &found {
        let attributes = decode_attributes(&rows::text(row, "attributes")?)
            .ok_or(PageError::Fallback("unusual inline comment attributes"))?;
        let mut state_now = match attributes.get("inline.state") {
            Some(Value::Object(map)) => ContentState::read(map)?,
            _ => ContentState {
                text: String::new(),
                has_suggestion: false,
                suggestion: String::new(),
            },
        };
        state_now.text = rows::opt_text(row, "content")?.unwrap_or_default();
        if attributes.get("editing").is_some_and(php::is_truthy) {
            let phid = rows::text(row, "phid")?;
            // loadDrafts(): the draft of the highest version.
            if let Some((_, properties)) = newest_draft(state, &phid, viewer.as_str()).await?
                && let Some(Value::Object(map)) = properties.get("inline.state")
            {
                state_now = ContentState::read(map)?;
            }
        }
        if !state_now.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `saveComment()`: the comment's row as `getTransactionCommentForSave()`
/// saves it, the purge of its versioned drafts, then the draft engine.
async fn save<S: WebState>(
    state: &S,
    inline: Inline,
    revision: &Revision,
    viewer: &Phid,
) -> Result<(), PageError> {
    write_row(state, &inline).await?;
    let database = state.database();
    let purge = format!(
        "DELETE FROM {} WHERE objectPHID = ? AND authorPHID = ?",
        database.table("draft", "draft_versioneddraft")
    );
    phab_db::sqlx::query(&purge)
        .bind(inline.phid.as_str())
        .bind(viewer.as_str())
        .execute(database.pool())
        .await
        .map_err(database_error)?;
    synchronize(state, revision, viewer).await
}

/// `DifferentialInlineComment::save()`: the policies, the legacy content
/// source and version 1, with the comment's values.
async fn write_row<S: WebState>(state: &S, inline: &Inline) -> Result<(), PageError> {
    let database = state.database();
    let sql = format!(
        "UPDATE {} SET viewPolicy = 'public', editPolicy = ?, contentSource = ?, \
         commentVersion = 1, content = ?, isDeleted = ?, fixedState = ?, attributes = ?, \
         dateModified = ? WHERE id = ?",
        database.table("differential", "differential_transaction_comment")
    );
    phab_db::sqlx::query(&sql)
        .bind(inline.author_phid.as_str())
        .bind(LEGACY_SOURCE)
        .bind(&inline.content)
        .bind(i64::from(inline.is_deleted))
        .bind(inline.fixed_state.as_deref())
        .bind(encode_attributes(inline.attributes.clone()))
        .bind(state.now())
        .bind(inline.id)
        .execute(database.pool())
        .await
        .map_err(database_error)?;
    Ok(())
}

/// The next state of "Done" and the answer's `isChecked` and
/// `draftState`.
fn next_done_state(state: Option<&str>) -> (&'static str, bool, bool) {
    match state {
        Some("draft") => ("undone", false, false),
        Some("undraft") => ("done", true, false),
        Some("done") => ("undraft", false, true),
        _ => ("draft", true, true),
    }
}

/// `getInt()` of a parameter which must be an integer for Rust: absent
/// is `None`, like PHP's `null`.
fn int_param(request: &LoginRequest, name: &str) -> Result<Option<i64>, PageError> {
    match request.str(name) {
        None => Ok(None),
        Some(value) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
            value
                .parse()
                .map(Some)
                .map_err(|_| PageError::Fallback("IDs which are not integers"))
        }
        Some(_) => Err(PageError::Fallback("IDs which are not integers")),
    }
}

/// `buildEmptyResponse()`.
fn empty_response() -> Response {
    crate::diffusion::ajax_response(&json!({"inline": [], "view": null}))
}

/// `POST /differential/comment/inline/edit/<id>/`.
pub(crate) async fn edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let revision_id: i64 = id
        .parse()
        .map_err(|_| PageError::Fallback("unusual revision IDs"))?;
    let request = LoginRequest::new(method, uri, headers, &body);
    if !request.is_post() {
        return Err(PageError::Fallback("requests which aren't posts"));
    }
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    // validateCSRF(): PHP answers a bad token with a 404.
    let token = request
        .str("__csrf__")
        .filter(|token| php::is_truthy_str(token))
        .or_else(|| request.header(axum::http::HeaderName::from_static("x-phabricator-csrf")))
        .unwrap_or_default();
    if !viewer.is_valid_csrf_token(token, state.now())? {
        return Err(PageError::Fallback("invalid CSRF tokens"));
    }
    if request.nonempty_str("replyToCommentPHID").is_some() {
        return Err(PageError::Fallback("replies to inline comments"));
    }
    let op = request.str("op").unwrap_or_default();
    match op {
        "hide" | "show" => hide_or_show(state, &viewer, &request, op == "hide").await,
        "done" => done(state, &viewer, &request).await,
        "delete" | "undelete" | "refdelete" | "cancel" | "draft" => {
            edit_draft(state, &viewer, &request, op, revision_id).await
        }
        _ => Err(PageError::Fallback("inline comment operations not ported")),
    }
}

/// `hide` and `show`: the viewer's hidden comments, by ID
/// (`getStrList('ids')`, each as `%d`).
async fn hide_or_show<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
    hide: bool,
) -> Result<Response, PageError> {
    let ids: Vec<i64> = request
        .str("ids")
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|id| !id.is_empty())
        .map(|id| {
            id.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| id.parse().ok())
                .flatten()
                .ok_or(PageError::Fallback("IDs which are not integers"))
        })
        .collect::<Result<_, _>>()?;
    if !ids.is_empty() {
        let database = state.database();
        let actor = viewer.user.phid.as_str();
        let table = database.table("differential", "differential_hiddencomment");
        let sql = if hide {
            format!(
                "INSERT IGNORE INTO {table} (userPHID, commentID) VALUES {}",
                vec!["(?, ?)"; ids.len()].join(", ")
            )
        } else {
            format!(
                "DELETE FROM {table} WHERE userPHID = ? AND commentID IN ({})",
                phab_db::placeholders(ids.len())
            )
        };
        let mut query = phab_db::sqlx::query(&sql);
        if hide {
            for id in &ids {
                query = query.bind(actor).bind(*id);
            }
        } else {
            query = query.bind(actor);
            for id in &ids {
                query = query.bind(*id);
            }
        }
        query
            .execute(database.pool())
            .await
            .map_err(database_error)?;
    }
    Ok(crate::diffusion::ajax_response(&json!([])))
}

/// `done`: the next state of the comment's "Done" checkbox.
async fn done<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
) -> Result<Response, PageError> {
    let comment_id =
        int_param(request, "id")?.ok_or(PageError::Fallback("inline comments which don't load"))?;
    let mut inline = load_for_done(state, viewer, comment_id).await?;
    let (next, is_checked, draft_state) = next_done_state(inline.fixed_state.as_deref());
    inline.fixed_state = Some(next.to_owned());
    write_row(state, &inline).await?;
    Ok(crate::diffusion::ajax_response(&json!({
        "isChecked": is_checked,
        "draftState": draft_state,
    })))
}

/// `delete`, `undelete`, `refdelete` (confirmed), `cancel` and `draft` of
/// the viewer's draft on revision `revision_id`.
async fn edit_draft<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
    op: &str,
    revision_id: i64,
) -> Result<Response, PageError> {
    // isFormPost(): otherwise the dialog asks first.
    let confirmed = request.exists("__form__") && !request.exists("__via__");
    if op == "refdelete" && !confirmed {
        return Err(PageError::Fallback("the confirmation of a deletion"));
    }
    if op == "cancel" && !request.bool("hasContentState") {
        // updateCommentContentState() throws.
        return Err(PageError::Fallback("cancels without a content state"));
    }
    let comment_id =
        int_param(request, "id")?.ok_or(PageError::Fallback("inline comments which don't load"))?;
    let actor = viewer.user.phid.clone();
    // The draft engine's object (getContainerObject()).
    let revision = visible_revision(state, viewer, revision_id).await?;
    let mut inline = load_for_edit(state, viewer, comment_id, &revision).await?;
    // The draft engine reads what Rust may leave to PHP: check it before
    // writing.
    has_draft_content(state, &revision, &actor).await?;
    match op {
        "delete" | "refdelete" => {
            inline.attributes.insert("editing".to_owned(), json!(false));
            inline.is_deleted = true;
        }
        "undelete" => inline.is_deleted = false,
        "cancel" => {
            inline.attributes.insert("editing".to_owned(), json!(false));
            let content = ContentState::from_request(request);
            inline.content.clone_from(&content.text);
            inline
                .attributes
                .insert("inline.state".to_owned(), content.storage_map());
        }
        _ => {
            write_versioned_draft(state, &inline, &actor, request).await?;
            synchronize(state, &revision, &actor).await?;
            return Ok(empty_response());
        }
    }
    save(state, inline, &revision, &actor).await?;
    Ok(empty_response())
}

/// `loadOrCreateDraft()` of the comment, with the editor's text as its
/// `inline.state` property.
async fn write_versioned_draft<S: WebState>(
    state: &S,
    inline: &Inline,
    actor: &Phid,
    request: &LoginRequest,
) -> Result<(), PageError> {
    let database = state.database();
    let table = database.table("draft", "draft_versioneddraft");
    let content = ContentState::from_request(request);
    let existing = newest_draft(state, inline.phid.as_str(), actor.as_str()).await?;
    let (id, mut properties) = if let Some(found) = existing {
        found
    } else {
        {
            let sql = format!(
                "INSERT INTO {table} (objectPHID, authorPHID, version, properties, dateCreated, \
                 dateModified) VALUES (?, ?, ?, '[]', ?, ?)"
            );
            let inserted = phab_db::sqlx::query(&sql)
                .bind(inline.phid.as_str())
                .bind(actor.as_str())
                .bind(inline.id)
                .bind(state.now())
                .bind(state.now())
                .execute(database.pool())
                .await
                .map_err(database_error)?;
            (
                i64::try_from(inserted.last_insert_id()).map_err(internal)?,
                Map::new(),
            )
        }
    };
    properties.insert("inline.state".to_owned(), content.storage_map());
    let sql = format!("UPDATE {table} SET properties = ?, dateModified = ? WHERE id = ?");
    phab_db::sqlx::query(&sql)
        .bind(encode_attributes(properties))
        .bind(state.now())
        .bind(id)
        .execute(database.pool())
        .await
        .map_err(database_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_done_states_like_php() {
        assert_eq!(next_done_state(Some("draft")), ("undone", false, false));
        assert_eq!(next_done_state(Some("undraft")), ("done", true, false));
        assert_eq!(next_done_state(Some("done")), ("undraft", false, true));
        assert_eq!(next_done_state(Some("undone")), ("draft", true, true));
        assert_eq!(next_done_state(None), ("draft", true, true));
    }

    #[test]
    fn reads_content_states_like_php() {
        let map = json!({"text": 5, "hasSuggestion": "1", "suggestionText": null});
        let state = ContentState::read(map.as_object().unwrap()).unwrap();
        assert_eq!(state.text, "5");
        assert!(state.has_suggestion);
        assert_eq!(state.suggestion, "");
        assert!(!state.is_empty());
        assert!(
            ContentState {
                text: String::new(),
                has_suggestion: true,
                suggestion: String::new(),
            }
            .is_empty()
        );
        assert!(ContentState::read(json!({"text": [1]}).as_object().unwrap()).is_err());
    }
}
