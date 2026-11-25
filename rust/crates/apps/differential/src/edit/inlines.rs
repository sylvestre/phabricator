//! Inline comments an edit publishes: the `inline` transactions of
//! `differential.revision.edit` (`DifferentialInlineCommentEditType`), and
//! the actor's draft inline comments, which the comment form publishes
//! (`newAutomaticInlineTransactions()`). Each is a `differential:inline`
//! transaction whose comment is the inline comment's row in
//! `differential_transaction_comment`: a new row, or the draft's, which
//! the transaction then owns. They come after the comment
//! (`sortTransactions()`).
//!
//! Like a comment, they subscribe their author (`applyImplicitCC()`), and
//! their text may mention users, reference objects and attach files.
//! Unlike a comment, they need `CAN_EDIT` (`getLegacyRequiredCapabilities()`).
//! A reply marks the comment it replies to as replied to
//! (`applyCustomExternalTransaction()`).
//!
//! Left to PHP: every value PHP refuses (with an error), inline comments
//! of new revisions, drafts being edited (PHP warns about them) and drafts
//! replying to comments which aren't published, paths which more than one
//! file of the diff has, and attributes PHP may encode back differently
//! (floats and big numbers).

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, PhidType, php};
use phab_db::rows;
use phab_db::sqlx::mysql::MySqlRow;
use serde_json::{Map, Value, json};

use super::Plan;
use crate::storage::Revision;

/// The keys of an `inline` transaction's value.
const KEYS: &[&str] = &[
    "diff",
    "path",
    "line",
    "length",
    "isNewFile",
    "content",
    "suggestionText",
    "replyToCommentPHID",
];

/// The keys a reply can't have: it goes on the lines of its parent.
const LOCATION_KEYS: &[&str] = &["diff", "path", "line", "length", "isNewFile"];

/// `DifferentialChangeType::FILE_DIRECTORY`.
const FILE_DIRECTORY: i64 = 4;

/// The largest value of the `uint32` line columns.
const MAX_LINE: i64 = 4_294_967_295;

/// PHP throws: the edit engine's errors are left to it.
fn refused() -> MethodError {
    MethodError::Unsupported("inline comments PHP refuses")
}

/// The text of an inline comment: its content, and the replacement of
/// "Suggest Edit".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Text {
    content: String,
    suggestion: Option<String>,
}

/// The diff a new inline comment goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DiffRef {
    /// The revision's active diff before the edit.
    Active,
    Id(i64),
    Phid(String),
}

/// An `inline` transaction's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Wanted {
    /// A comment on lines of a file of a diff.
    New {
        diff: DiffRef,
        path: String,
        line: i64,
        length: i64,
        is_new_file: bool,
        text: Text,
    },
    /// A reply to a published inline comment, on its lines.
    Reply { phid: String, text: Text },
}

/// `readInt()`: an integer, or the digits form-encoded calls send.
enum Int {
    Null,
    Value(i64),
    Invalid,
}

fn read_int(value: Option<&Value>) -> Int {
    match value {
        None | Some(Value::Null) => Int::Null,
        Some(Value::Number(number)) => number.as_i64().map_or(Int::Invalid, Int::Value),
        Some(Value::String(text))
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            // PHP saturates digits beyond its integers.
            text.parse().map_or(Int::Invalid, Int::Value)
        }
        Some(_) => Int::Invalid,
    }
}

/// `readBool()`, of `isNewFile`, which defaults to `true`.
fn read_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        None => Some(true),
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(text)) => match text.as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        Some(_) => None,
    }
}

/// `getTransactionValueFromConduit()` without the reads: the value of an
/// `inline` transaction, or what PHP refuses.
pub(super) fn parse(value: &Value) -> Result<Wanted, MethodError> {
    // A dictionary; one keyed `0..n` is a list, whose keys are unknown.
    let Value::Object(map) = value else {
        return Err(refused());
    };
    if map.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(refused());
    }
    // `idx($value, 'content', '')` is null for an explicit null.
    let content = match map.get("content") {
        None => String::new(),
        Some(Value::String(content)) => content.clone(),
        Some(_) => return Err(refused()),
    };
    let suggestion = match map.get("suggestionText") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(refused()),
    };
    if content.is_empty() && suggestion.as_deref().is_none_or(str::is_empty) {
        return Err(refused());
    }
    // The suggestion replaces whole lines: it ends with a newline.
    let suggestion = suggestion.map(|mut text| {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    });
    let text = Text {
        content,
        suggestion,
    };
    match map.get("replyToCommentPHID") {
        None | Some(Value::Null) => {}
        Some(Value::String(phid)) => {
            if LOCATION_KEYS.iter().any(|key| map.contains_key(*key)) {
                return Err(refused());
            }
            return Ok(Wanted::Reply {
                phid: phid.clone(),
                text,
            });
        }
        Some(_) => return Err(refused()),
    }
    let path = match map.get("path") {
        Some(Value::String(path)) if !path.is_empty() => path.clone(),
        _ => return Err(refused()),
    };
    let diff = match map.get("diff") {
        None | Some(Value::Null) => DiffRef::Active,
        Some(value) => match (read_int(Some(value)), value) {
            (Int::Value(id), _) => DiffRef::Id(id),
            (_, Value::String(phid)) if !phid.is_empty() => DiffRef::Phid(phid.clone()),
            _ => return Err(refused()),
        },
    };
    let line = match read_int(map.get("line")) {
        Int::Value(line) if line >= 1 => line,
        _ => return Err(refused()),
    };
    // An explicit null is no length.
    let length = match map.get("length") {
        None => 1,
        Some(value) => match read_int(Some(value)) {
            Int::Value(length) if length >= 1 => length,
            _ => return Err(refused()),
        },
    };
    let is_new_file = read_bool(map.get("isNewFile")).ok_or_else(refused)?;
    if line > MAX_LINE || length - 1 > MAX_LINE {
        return Err(MethodError::Unsupported("values longer than their column"));
    }
    Ok(Wanted::New {
        diff,
        path,
        line,
        length,
        is_new_file,
        text,
    })
}

/// A planned `differential:inline` transaction and its comment.
pub(super) struct Planned {
    /// The transaction.
    pub(super) phid: String,
    /// The draft's row ID; a new comment has none.
    draft: Option<i64>,
    /// The comment's PHID.
    comment_phid: String,
    changeset_id: i64,
    is_new_file: bool,
    line_number: i64,
    line_length: i64,
    reply_to: Option<String>,
    pub(super) content: String,
    /// The serialized attributes, as the comment is saved.
    attributes: String,
    /// The state of a new comment: the author's start out "done".
    fixed_state: Option<&'static str>,
}

/// The inline comments of an edit.
#[derive(Default)]
pub(super) struct Inlines {
    pub(super) planned: Vec<Planned>,
    /// The published comments replied to for the first time: their row ID
    /// and their attributes, as `save()` encodes them again.
    parents: Vec<(i64, String)>,
}

impl Inlines {
    /// The transactions, in the order PHP applies them.
    pub(super) fn phids(&self) -> Vec<String> {
        self.planned
            .iter()
            .map(|planned| planned.phid.clone())
            .collect()
    }

    /// The texts, for their remarkup effects.
    pub(super) fn texts(&self) -> Vec<String> {
        self.planned
            .iter()
            .map(|planned| planned.content.clone())
            .collect()
    }

    /// Records the reply to `parent`, which marks it replied to.
    fn reply_to(&mut self, parent: &Parent) -> Result<(), MethodError> {
        if parent.has_replies || self.parents.iter().any(|(id, _)| *id == parent.id) {
            return Ok(());
        }
        let attributes = decode_attributes(&parent.attributes).ok_or(MethodError::Unsupported(
            "unusual inline comment attributes",
        ))?;
        self.parents
            .push((parent.id, encode_attributes(attributes)));
        Ok(())
    }
}

/// A published inline comment replied to.
struct Parent {
    id: i64,
    changeset_id: i64,
    is_new_file: bool,
    line_number: i64,
    line_length: i64,
    has_replies: bool,
    attributes: String,
}

/// The table of the comments.
fn table(request: &ApiRequest<'_>) -> String {
    request
        .context
        .database
        .table("differential", "differential_transaction_comment")
}

/// `json_decode($attributes, true)` of a row's attributes: a dictionary.
/// `None` for what PHP may decode or encode back differently: invalid
/// JSON, lists, floats and integers beyond PHP's.
pub fn decode_attributes(text: &str) -> Option<Map<String, Value>> {
    fn plain(value: &Value) -> bool {
        match value {
            Value::Number(number) => number.is_i64(),
            Value::Array(list) => list.iter().all(plain),
            Value::Object(map) => map.values().all(plain),
            _ => true,
        }
    }
    let value: Value = serde_json::from_str(text).ok()?;
    if !plain(&value) {
        return None;
    }
    match php::round_trip(&value) {
        Value::Object(map) => Some(map),
        Value::Array(list) if list.is_empty() => Some(Map::new()),
        _ => None,
    }
}

/// `phutil_json_encode()` of attributes: an empty array is a list.
pub fn encode_attributes(attributes: Map<String, Value>) -> String {
    if attributes.is_empty() {
        return "[]".to_owned();
    }
    php::json_encode(&Value::Object(attributes))
}

/// The content state of a comment (`newStorageMap()` of
/// `PhabricatorDiffInlineCommentContentState`).
fn content_state(content: &str, has_suggestion: bool, suggestion: &str) -> Value {
    json!({
        "text": content,
        "hasSuggestion": has_suggestion,
        "suggestionText": suggestion,
    })
}

fn parent_from_row(row: &MySqlRow) -> Result<(Parent, Option<String>, String), MethodError> {
    let parent = Parent {
        id: rows::int(row, "id")?,
        changeset_id: rows::int(row, "changesetID")?,
        is_new_file: rows::flag(row, "isNewFile")?,
        line_number: rows::int(row, "lineNumber")?,
        line_length: rows::int(row, "lineLength")?,
        has_replies: rows::flag(row, "hasReplies")?,
        attributes: rows::text(row, "attributes")?,
    };
    let transaction = rows::opt_text(row, "transactionPHID")?;
    Ok((parent, transaction, rows::text(row, "viewPolicy")?))
}

/// The inline comment `phid`, if it is published; on `revision` for
/// `inline` transactions (`withRevisionPHIDs()`), anywhere for the parent
/// of a draft (`needReplyToComments()`). Comments are public; others are
/// left to PHP.
async fn load_parent(
    request: &ApiRequest<'_>,
    phid: &str,
    revision: Option<&Revision>,
) -> Result<Option<Parent>, MethodError> {
    let database = &request.context.database;
    let on_revision = if revision.is_some() {
        " AND revisionPHID = ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, transactionPHID, viewPolicy, changesetID, isNewFile, lineNumber, \
         lineLength, hasReplies, attributes FROM {} WHERE phid = ? AND changesetID IS NOT \
         NULL{on_revision}",
        table(request)
    );
    let mut query = phab_db::sqlx::query(&sql).bind(phid);
    if let Some(revision) = revision {
        query = query.bind(revision.phid.as_str());
    }
    let Some(row) = query.fetch_optional(database.pool()).await? else {
        return Ok(None);
    };
    let (parent, transaction, policy) = parent_from_row(&row)?;
    if policy != "public" {
        return Err(MethodError::Unsupported(
            "inline comments with unusual policies",
        ));
    }
    Ok(transaction.map(|_| parent))
}

/// `loadDiff()` and `findChangeset()`: the changeset of `path` in the diff
/// of `revision` `diff` names.
async fn find_changeset(
    request: &ApiRequest<'_>,
    revision: &Revision,
    diff: &DiffRef,
    path: &str,
) -> Result<i64, MethodError> {
    let database = &request.context.database;
    let diffs = database.table("differential", "differential_diff");
    let (column, value) = match diff {
        DiffRef::Active => {
            let Some(active) = &revision.active_diff_phid else {
                return Err(refused());
            };
            ("phid", active.as_str().to_owned())
        }
        DiffRef::Id(id) => ("id", id.to_string()),
        DiffRef::Phid(phid) => ("phid", phid.clone()),
    };
    let sql = format!("SELECT id FROM {diffs} WHERE revisionID = ? AND {column} = ?");
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(revision.id)
        .bind(&value)
        .fetch_optional(database.pool())
        .await?
    else {
        return Err(refused());
    };
    let diff_id = rows::int(&row, "id")?;
    let sql = format!(
        "SELECT id, filename, fileType FROM {} WHERE diffID = ?",
        database.table("differential", "differential_changeset")
    );
    let changesets: Vec<(i64, String, i64)> = phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                rows::int(row, "id")?,
                rows::text(row, "filename")?,
                rows::int(row, "fileType")?,
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()?;
    // The file name, else the display name (directories end with `/`).
    let display = |(_, filename, file_type): &&(i64, String, i64)| {
        *file_type == FILE_DIRECTORY && path.strip_suffix('/') == Some(filename.as_str())
    };
    let by_name: Vec<&(i64, String, i64)> = changesets
        .iter()
        .filter(|(_, filename, _)| filename == path)
        .collect();
    let found = if by_name.is_empty() {
        changesets.iter().filter(display).collect()
    } else {
        by_name
    };
    match found.as_slice() {
        [(id, ..)] => Ok(*id),
        [] => Err(refused()),
        // PHP takes the first in its query's order.
        _ => Err(MethodError::Unsupported("paths of several files of a diff")),
    }
}

/// The `inline` transactions of the request, on `revision` before the
/// edit, by `actor`.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    wanted: &[Wanted],
    actor: &str,
) -> Result<Inlines, MethodError> {
    let mut inlines = Inlines::default();
    let is_author = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor);
    for item in wanted {
        let (text, reply_to, (changeset_id, is_new_file, line_number, line_length)) = match item {
            Wanted::Reply { phid, text } => {
                let parent = load_parent(request, phid, Some(revision))
                    .await?
                    .ok_or_else(refused)?;
                inlines.reply_to(&parent)?;
                let location = (
                    parent.changeset_id,
                    parent.is_new_file,
                    parent.line_number,
                    parent.line_length,
                );
                (text, Some(phid.clone()), location)
            }
            Wanted::New {
                diff,
                path,
                line,
                length,
                is_new_file,
                text,
            } => {
                let changeset = find_changeset(request, revision, diff, path).await?;
                // Storage counts the lines after the first.
                (text, None, (changeset, *is_new_file, *line, length - 1))
            }
        };
        let state = content_state(
            &text.content,
            text.suggestion.is_some(),
            text.suggestion.as_deref().unwrap_or_default(),
        );
        let mut attributes = Map::new();
        attributes.insert("inline.state".to_owned(), state);
        attributes.insert("editing".to_owned(), json!(false));
        inlines.planned.push(Planned {
            phid: super::transaction_phid(),
            draft: None,
            comment_phid: Phid::generate(PhidType::TRANSACTION_COMMENT, None).into_string(),
            changeset_id,
            is_new_file,
            line_number,
            line_length,
            reply_to,
            content: text.content.clone(),
            attributes: encode_attributes(attributes),
            fixed_state: is_author.then_some("done"),
        });
    }
    Ok(inlines)
}

/// `(string)` of a content state's suggestion, for the values PHP casts
/// the same.
fn suggestion_text(value: Option<&Value>) -> Result<String, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(value @ (Value::String(_) | Value::Bool(_) | Value::Number(_))) => {
            Ok(php::string_cast(value).into_owned())
        }
        Some(_) => Err(MethodError::Unsupported(
            "unusual inline comment attributes",
        )),
    }
}

/// Whether the actor has a versioned draft of the inline comment `phid`
/// (`loadAndAttachVersionedDrafts()`).
pub(super) async fn has_versioned_draft(
    request: &ApiRequest<'_>,
    phid: &str,
    actor: &str,
) -> Result<bool, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE objectPHID = ? AND authorPHID = ? LIMIT 1",
        database.table("draft", "draft_versioneddraft")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .bind(actor)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// The actor's drafts on `revision` the comment form publishes
/// (`withPublishableComments()`, `needAppliedDrafts()`,
/// `needReplyToComments()`), in the order of their IDs: those with text or
/// a suggestion, which get the content state of their text.
pub(super) async fn drafts(
    request: &ApiRequest<'_>,
    revision: &Revision,
    actor: &str,
) -> Result<Inlines, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id, phid, viewPolicy, content, changesetID, isNewFile, lineNumber, \
         lineLength, replyToCommentPHID, attributes FROM {} WHERE changesetID IS NOT NULL AND \
         revisionPHID = ? AND authorPHID = ? AND isDeleted = 0 AND transactionPHID IS NULL \
         ORDER BY id",
        table(request)
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(actor)
        .fetch_all(database.pool())
        .await?;
    let unusual = || MethodError::Unsupported("unusual inline comment attributes");
    let mut inlines = Inlines::default();
    for row in &found {
        if rows::text(row, "viewPolicy")? != "public" {
            return Err(MethodError::Unsupported(
                "inline comments with unusual policies",
            ));
        }
        let mut attributes =
            decode_attributes(&rows::text(row, "attributes")?).ok_or_else(unusual)?;
        let phid = rows::text(row, "phid")?;
        let is_editing = attributes.get("editing").is_some_and(php::is_truthy);
        // The text of a draft being edited is its versioned draft's.
        if is_editing && has_versioned_draft(request, &phid, actor).await? {
            return Err(MethodError::Unsupported("inline comments being edited"));
        }
        let content = rows::text(row, "content")?;
        let (has_suggestion, suggestion) = match attributes.get("inline.state") {
            Some(Value::Object(state)) => (
                state.get("hasSuggestion").is_some_and(php::is_truthy),
                suggestion_text(state.get("suggestionText"))?,
            ),
            _ => (false, String::new()),
        };
        // isVoidComment(): an empty draft stays a draft.
        if content.is_empty() && (!has_suggestion || suggestion.is_empty()) {
            continue;
        }
        // hasWarnings(): PHP asks whether to publish what is being edited.
        if is_editing {
            return Err(MethodError::Unsupported("inline comments being edited"));
        }
        let reply_to = rows::opt_text(row, "replyToCommentPHID")?.filter(|phid| !phid.is_empty());
        if let Some(phid) = &reply_to {
            // A draft replying to another draft would be saved over by its
            // parent's save; one without its parent is not published.
            let parent =
                load_parent(request, phid, None)
                    .await?
                    .ok_or(MethodError::Unsupported(
                        "drafts replying to unpublished inline comments",
                    ))?;
            inlines.reply_to(&parent)?;
        }
        attributes.insert(
            "inline.state".to_owned(),
            content_state(&content, has_suggestion, &suggestion),
        );
        attributes.insert("editing".to_owned(), json!(false));
        inlines.planned.push(Planned {
            phid: super::transaction_phid(),
            draft: Some(rows::int(row, "id")?),
            comment_phid: phid,
            changeset_id: rows::int(row, "changesetID")?,
            is_new_file: rows::flag(row, "isNewFile")?,
            line_number: rows::int(row, "lineNumber")?,
            line_length: rows::int(row, "lineLength")?,
            reply_to,
            content,
            attributes: encode_attributes(attributes),
            fixed_state: None,
        });
    }
    Ok(inlines)
}

/// The transactions and their comments, the drafts' versioned drafts
/// (`purgeDrafts()`), then the parents replied to (`setHasReplies()`).
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let table = table(request);
    let source = plan.source.serialized();
    for planned in &plan.inlines.planned {
        match planned.draft {
            Some(id) => {
                phab_db::sqlx::query(&format!(
                    "UPDATE {table} SET transactionPHID = ?, authorPHID = ?, viewPolicy = \
                     'public', editPolicy = ?, commentVersion = 1, contentSource = ?, \
                     attributes = ?, dateModified = ? WHERE id = ?"
                ))
                .bind(&planned.phid)
                .bind(&plan.actor)
                .bind(&plan.actor)
                .bind(source)
                .bind(&planned.attributes)
                .bind(now)
                .bind(id)
                .execute(&mut *connection)
                .await?;
            }
            None => {
                phab_db::sqlx::query(&format!(
                    "INSERT INTO {table} (phid, transactionPHID, authorPHID, viewPolicy, \
                     editPolicy, commentVersion, content, contentSource, isDeleted, dateCreated, \
                     dateModified, revisionPHID, changesetID, isNewFile, lineNumber, lineLength, \
                     fixedState, hasReplies, replyToCommentPHID, attributes) VALUES (?, ?, ?, \
                     'public', ?, 1, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)"
                ))
                .bind(&planned.comment_phid)
                .bind(&planned.phid)
                .bind(&plan.actor)
                .bind(&plan.actor)
                .bind(&planned.content)
                .bind(source)
                .bind(now)
                .bind(now)
                .bind(plan.revision.phid.as_str())
                .bind(planned.changeset_id)
                .bind(i64::from(planned.is_new_file))
                .bind(planned.line_number)
                .bind(planned.line_length)
                .bind(planned.fixed_state)
                .bind(&planned.reply_to)
                .bind(&planned.attributes)
                .execute(&mut *connection)
                .await?;
            }
        }
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
             commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
             dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, ?, 1, \
             'differential:inline', 'null', 'null', ?, ?, ?, ?)",
            database.table("differential", "differential_transaction")
        ))
        .bind(&planned.phid)
        .bind(&plan.actor)
        .bind(plan.revision.phid.as_str())
        .bind(&plan.actor)
        .bind(&planned.comment_phid)
        .bind(source)
        .bind(metadata)
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
        // A new comment has no PHID yet when its drafts are purged.
        if planned.draft.is_some() {
            phab_db::sqlx::query(&format!(
                "DELETE FROM {} WHERE objectPHID = ? AND authorPHID = ?",
                database.table("draft", "draft_versioneddraft")
            ))
            .bind(&planned.comment_phid)
            .bind(&plan.actor)
            .execute(&mut *connection)
            .await?;
        }
    }
    for (id, attributes) in &plan.inlines.parents {
        phab_db::sqlx::query(&format!(
            "UPDATE {table} SET hasReplies = 1, attributes = ?, dateModified = ? WHERE id = ?"
        ))
        .bind(attributes)
        .bind(now)
        .bind(id)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(content: &str, suggestion: Option<&str>) -> Text {
        Text {
            content: content.to_owned(),
            suggestion: suggestion.map(str::to_owned),
        }
    }

    #[test]
    fn reads_values_like_php() {
        assert_eq!(
            parse(
                &json!({"path": "fruit", "line": 2, "length": 2, "content": "Hm",
                "suggestionText": "banana\ncherry"})
            ),
            Ok(Wanted::New {
                diff: DiffRef::Active,
                path: "fruit".to_owned(),
                line: 2,
                length: 2,
                is_new_file: true,
                text: text("Hm", Some("banana\ncherry\n")),
            })
        );
        // Form-encoded values are strings.
        assert_eq!(
            parse(
                &json!({"diff": "7", "path": "basket", "line": "2", "isNewFile": "false",
                "content": "Old side."})
            ),
            Ok(Wanted::New {
                diff: DiffRef::Id(7),
                path: "basket".to_owned(),
                line: 2,
                length: 1,
                is_new_file: false,
                text: text("Old side.", None),
            })
        );
        assert_eq!(
            parse(
                &json!({"diff": "PHID-DIFF-x", "path": "a", "line": 1, "suggestionText": ""
                , "content": "Delete"})
            ),
            Ok(Wanted::New {
                diff: DiffRef::Phid("PHID-DIFF-x".to_owned()),
                path: "a".to_owned(),
                line: 1,
                length: 1,
                is_new_file: true,
                text: text("Delete", Some("")),
            })
        );
        assert_eq!(
            parse(&json!({"replyToCommentPHID": "PHID-XCMT-1", "content": "Fixed."})),
            Ok(Wanted::Reply {
                phid: "PHID-XCMT-1".to_owned(),
                text: text("Fixed.", None),
            })
        );
    }

    #[test]
    fn refuses_what_php_refuses() {
        for value in [
            json!(["fruit"]),
            json!({"path": "fruit", "line": 1, "content": "x", "lineNumber": 1}),
            json!({"line": 1, "content": "x"}),
            json!({"path": "fruit", "content": "x"}),
            json!({"path": "fruit", "line": 0, "content": "x"}),
            json!({"path": "fruit", "line": 1, "length": 0, "content": "x"}),
            json!({"path": "fruit", "line": 1, "length": null, "content": "x"}),
            json!({"path": "fruit", "line": 1.0, "content": "x"}),
            json!({"path": "fruit", "line": 1, "isNewFile": "yes", "content": "x"}),
            json!({"path": "fruit", "line": 1, "isNewFile": 1, "content": "x"}),
            json!({"path": "fruit", "line": 1}),
            json!({"path": "fruit", "line": 1, "suggestionText": ""}),
            json!({"path": "fruit", "line": 1, "content": null}),
            json!({"path": "fruit", "line": 1, "content": "x", "diff": ""}),
            json!({"replyToCommentPHID": "PHID-XCMT-1", "content": "x", "path": "fruit"}),
            json!({"replyToCommentPHID": "PHID-XCMT-1", "content": "x", "diff": null}),
            json!({"replyToCommentPHID": 1, "content": "x"}),
        ] {
            assert_eq!(parse(&value), Err(refused()), "{value}");
        }
    }

    #[test]
    fn attributes_encode_back_like_php() {
        let decoded = decode_attributes(r#"{"editing":true,"inline.state":{}}"#).unwrap();
        assert_eq!(
            encode_attributes(decoded),
            r#"{"editing":true,"inline.state":[]}"#
        );
        assert_eq!(encode_attributes(decode_attributes("[]").unwrap()), "[]");
        assert_eq!(encode_attributes(decode_attributes("{}").unwrap()), "[]");
        assert!(decode_attributes(r#"{"x":1.5}"#).is_none());
        assert!(decode_attributes("[1]").is_none());
        assert!(decode_attributes("nope").is_none());
    }
}
