//! The frozen Differential write methods, which build transactions and
//! hand them to the editor: `differential.createcomment`
//! (`DifferentialCreateCommentConduitAPIMethod`),
//! `differential.createrevision` and `differential.updaterevision`
//! (`DifferentialCreateRevisionConduitAPIMethod`,
//! `DifferentialUpdateRevisionConduitAPIMethod`, through
//! `DifferentialConduitAPIMethod::applyFieldEdit()`) and
//! `differential.close` (`DifferentialCloseConduitAPIMethod`).
//!
//! Each loads what PHP loads, answers PHP's errors, then translates its
//! parameters into the `differential.revision.edit` transactions PHP's
//! transactions amount to, in PHP's order, and runs [`edit::apply()`] with
//! the Conduit content source. The legacy actions `createcomment` and
//! `close` apply are the edit engine's: the same transaction types with
//! the value `true`, a `TYPE_COMMENT` with a fresh comment, on an editor
//! which continues on missing fields and on no effect. What the editor
//! leaves to PHP stays PHP's, before anything is written.
//!
//! Left to PHP besides: revisions the viewer can see but not edit, or not
//! see (PHP raises a policy exception), `attach_inlines` when the viewer
//! has draft inline comments on the revision, parameter values of a type
//! PHP would convert or choke on, and calls whose writes are skipped.

use phab_conduit::pager::core_error;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{php, text};
use phab_db::Database;
use serde_json::{Map, Value, json};

use crate::commit_message::fields::{self, Field, Kind};
use crate::edit::{self, FileFinder, Source};
use crate::storage::{self, Revision};

const ERR_BAD_REVISION: &str = "ERR_BAD_REVISION";
const ERR_BAD_DIFF: &str = "ERR_BAD_DIFF";
const ERR_CLOSED: &str = "ERR_CLOSED";
const ERR_NOT_FOUND: &str = "ERR_NOT_FOUND";

fn conduit_error(code: &str) -> MethodError {
    MethodError::Conduit {
        code: code.to_owned(),
        info: None,
    }
}

/// In shadow mode and on read-only installs writes are skipped, so the
/// call is PHP's alone.
fn check_writes(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    if request.writes != phab_auth::Writes::Allowed {
        return Err(MethodError::Unsupported("writes when they are skipped"));
    }
    Ok(())
}

/// An ID for `withIDs()`: an integer or its digits.
fn object_id(value: Option<&Value>) -> Result<i64, MethodError> {
    match value {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text))
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            text.parse().ok()
        }
        _ => None,
    }
    .filter(|id| *id >= 0)
    .ok_or(MethodError::Unsupported("IDs which are not integers"))
}

/// `DifferentialRevisionQuery::withIDs(array($id))->executeOne()`, with
/// `requireCapabilities()` of `CAN_EDIT` when `edit`: the revision, `None`
/// when it doesn't exist. Hidden revisions raise a policy exception,
/// which is left to PHP.
async fn load_revision(
    request: &ApiRequest<'_>,
    id: i64,
    edit: bool,
) -> Result<Option<Revision>, MethodError> {
    let database = &request.context.database;
    let config = &request.context.config;
    let Some(revision) = storage::load_revisions_by_id(database, &[id])
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let visible = storage::visible_revisions(
        database,
        config,
        &request.viewer,
        vec![revision.clone()],
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if visible.is_empty() {
        return Err(MethodError::Unsupported(
            "revisions the viewer can't see, a policy exception",
        ));
    }
    if edit
        && !storage::can_edit(database, config, &request.viewer, &revision, request.now)
            .await
            .map_err(MethodError::internal)?
    {
        return Err(MethodError::Unsupported(
            "revisions the viewer can't edit, a policy exception",
        ));
    }
    Ok(Some(revision))
}

/// Runs the editor on the `transactions` of the call, on revision
/// `revision_id` or on a new one, as `differential.revision.edit` from
/// Conduit: the nested `ConduitCall` of `applyFieldEdit()` is strictly
/// typed.
async fn apply(
    request: &ApiRequest<'_>,
    files: Option<FileFinder>,
    revision_id: Option<i64>,
    transactions: Vec<Value>,
) -> Result<Value, MethodError> {
    let mut params = Map::new();
    params.insert("transactions".to_owned(), Value::Array(transactions));
    if let Some(id) = revision_id {
        params.insert("objectIdentifier".to_owned(), json!(id));
    }
    let nested = ApiRequest {
        context: request.context,
        user: request.user.clone(),
        viewer: request.viewer.clone(),
        params,
        strictly_typed: true,
        now: request.now,
        writes: request.writes,
    };
    edit::apply(&nested, files, Source::Conduit).await
}

/// The answer of `createcomment`, `createrevision` and `updaterevision`:
/// `id` as PHP has it (Lisk's string for a loaded revision, the edit
/// engine's integer for a new one).
fn revision_answer(
    request: &ApiRequest<'_>,
    id: i64,
    id_value: &Value,
) -> Result<Value, MethodError> {
    let uri = request.context.config.uri(&format!("/D{id}"))?;
    Ok(json!({"revisionid": id_value, "uri": uri}))
}

/// The edit engine's types of the legacy actions with a modular
/// transaction (`$modular_map`): accept, reject, resign,
/// `request_review`, rethink.
fn action_type(action: &str) -> Option<&'static str> {
    Some(match action {
        "accept" => "accept",
        "reject" => "reject",
        "resign" => "resign",
        "request_review" => "request-review",
        "rethink" => "plan-changes",
        _ => return None,
    })
}

/// `withHasTransaction(false)` and `withIsDeleted(false)`: whether the
/// viewer has draft inline comments on the revision, which
/// `attach_inlines` submits (`loadUnsubmittedInlineComments()`).
async fn has_draft_inlines(
    database: &Database,
    revision: &Revision,
    viewer: &str,
) -> Result<bool, MethodError> {
    let sql = format!(
        "SELECT id FROM {} WHERE revisionPHID = ? AND authorPHID = ? AND transactionPHID IS NULL \
         AND isDeleted = 0 LIMIT 1",
        database.table("differential", "differential_transaction_comment")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(viewer)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// `differential.createcomment`.
#[derive(Default)]
pub struct CreateComment {
    files: Option<FileFinder>,
}

impl CreateComment {
    /// The method, whose comments find referenced files with `files`.
    pub fn with_files(files: FileFinder) -> Self {
        Self { files: Some(files) }
    }

    async fn run(&self, request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        check_writes(request)?;
        let id = object_id(request.value("revision_id"))?;
        let revision = load_revision(request, id, false)
            .await?
            .ok_or_else(|| conduit_error(ERR_BAD_REVISION))?;

        let mut transactions = Vec::new();
        match request.value("action") {
            None | Some(Value::Null) => {}
            Some(Value::String(action)) => match action_type(action) {
                Some(kind) => transactions.push(json!({"type": kind, "value": true})),
                None if !php::is_truthy_str(action) => {}
                None if matches!(action.as_str(), "comment" | "none") => {}
                None => return Err(core_error(format!("Unsupported action \"{action}\"."))),
            },
            // `isset($modular_map[$action])` and `switch` compare loosely.
            Some(_) => return Err(MethodError::Unsupported("actions which are not strings")),
        }
        match request.value("message") {
            None | Some(Value::Null) => {}
            Some(Value::String(message)) if message.is_empty() => {}
            Some(Value::String(message)) => {
                transactions.push(json!({"type": "comment", "value": message}));
            }
            Some(_) => return Err(MethodError::Unsupported("messages which are not strings")),
        }
        if request.value("attach_inlines").is_some_and(php::is_truthy) {
            let viewer = request.viewer.phid.as_ref();
            let viewer = viewer.ok_or(MethodError::Unsupported("edits by the omnipotent user"))?;
            if has_draft_inlines(&request.context.database, &revision, viewer.as_str()).await? {
                return Err(MethodError::Unsupported("draft inline comments"));
            }
        }
        // The legacy "silent" flag has no effect (T13042).
        apply(request, self.files, Some(revision.id), transactions).await?;
        revision_answer(request, revision.id, &json!(revision.id.to_string()))
    }
}

impl Method for CreateComment {
    fn name(&self) -> &'static str {
        "differential.createcomment"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "revision_id",
            "message",
            "action",
            "silent",
            "attach_inlines",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[(ERR_BAD_REVISION, "Bad revision ID.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(self.run(request))
    }
}

/// PHP's `gettype()` of a decoded JSON value.
fn php_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        // PHP decodes integers beyond `PHP_INT_MAX` as floats.
        Value::Number(number) if !number.is_i64() => "double",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) | Value::Object(_) => "array",
    }
}

/// `readFieldValueFromConduit()`.
fn read_field_value(field: Field, value: &Value) -> Result<Value, MethodError> {
    let key = field.key();
    let value = match (field.kind(), value) {
        // `(string)$value` of integers.
        (Kind::RevisionId, Value::Number(number)) if number.is_i64() => {
            Value::String(number.to_string())
        }
        _ => value.clone(),
    };
    match field.kind() {
        Kind::Text | Kind::RevisionId => match value {
            Value::Null | Value::String(_) => Ok(value),
            other => Err(core_error(format!(
                "Field \"{key}\" expects a string value, but received a value of type \"{}\".",
                php_type(&other)
            ))),
        },
        Kind::List => match value {
            Value::Array(_) => Ok(value),
            Value::Object(_) => Err(MethodError::Unsupported("lists which are dictionaries")),
            other => Err(core_error(format!(
                "Field \"{key}\" expects a list of strings, but received a value of type \"{}\".",
                php_type(&other)
            ))),
        },
    }
}

/// `getFieldTransactions()`: the edit engine transactions of a field.
fn field_transactions(field: Field, value: Value) -> Result<Vec<Value>, MethodError> {
    let kind = match field {
        Field::Title => "title",
        Field::Summary => "summary",
        Field::TestPlan => "testPlan",
        Field::Reviewers => {
            // `inflateReviewers()`: a trailing "!" makes a blocking reviewer.
            let Value::Array(reviewers) = value else {
                return Err(MethodError::Unsupported("lists which are dictionaries"));
            };
            let mut list = Vec::new();
            for reviewer in reviewers {
                let Value::String(reviewer) = reviewer else {
                    return Err(MethodError::Unsupported("reviewers which are not strings"));
                };
                list.push(match reviewer.strip_suffix('!') {
                    Some(phid) => json!(format!("blocking({phid})")),
                    None => json!(reviewer),
                });
            }
            return Ok(vec![json!({"type": "reviewers.set", "value": list})]);
        }
        Field::Subscribers => "subscribers.set",
        Field::Tags => "projects.set",
        // `DifferentialCommitMessageCustomField`: the field's own key.
        Field::Auditors | Field::BugId | Field::BlameRevision | Field::RevertPlan => field.key(),
        Field::RevisionId | Field::ReviewedBy | Field::Conflicts | Field::GitSvnId => {
            return Ok(Vec::new());
        }
    };
    Ok(vec![json!({"type": kind, "value": value})])
}

/// The transactions `applyFieldEdit()` builds before the message: the
/// diff, then those of each known field of `fields`, in their order there.
fn field_edit_transactions(
    request: &ApiRequest<'_>,
    diff_phid: &str,
) -> Result<Vec<Value>, MethodError> {
    let mut transactions = vec![json!({"type": "update", "value": diff_phid})];
    let enabled = fields::enabled_fields(&request.context.config)?;
    let values = match request.value("fields") {
        Some(Value::Object(values)) => values,
        // A list has integer keys, which name no field.
        None | Some(Value::Array(_)) => return Ok(transactions),
        Some(_) => {
            return Err(MethodError::Unsupported(
                "fields which are not dictionaries",
            ));
        }
    };
    for (key, value) in values {
        // Unknown fields, like read-only ones, are skipped.
        let Some(field) = enabled.iter().copied().find(|field| field.key() == key) else {
            continue;
        };
        let value = read_field_value(field, value)?;
        transactions.extend(field_transactions(field, value)?);
    }
    Ok(transactions)
}

/// `DifferentialDiffQuery::withIDs(array($diffid))->executeOne()`: the
/// diff's ID and PHID, or `ERR_BAD_DIFF`.
async fn load_diff(request: &ApiRequest<'_>) -> Result<(i64, String), MethodError> {
    let id = object_id(request.value("diffid"))?;
    let diff = crate::diff_search::execute_one(request, id)
        .await?
        .ok_or_else(|| conduit_error(ERR_BAD_DIFF))?;
    Ok((diff.id, diff.phid.as_str().to_owned()))
}

/// The diff's description `applyFieldEdit()` saves: the message's first
/// line, cut to 250 bytes and 80 glyphs.
fn diff_description(message: &str) -> String {
    let first = text::split_lines(message)
        .first()
        .copied()
        .unwrap_or_default();
    text::truncate(first, Some(250), Some(80))
}

/// `$diff->setDescription($first_line); $diff->save()`.
async fn save_diff_description(
    request: &ApiRequest<'_>,
    diff_id: i64,
    description: &str,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET description = ?, dateModified = ? WHERE id = ?",
        database.table("differential", "differential_diff")
    ))
    .bind(description)
    .bind(request.now)
    .bind(diff_id)
    .execute(database.pool())
    .await?;
    Ok(())
}

/// `differential.createrevision`.
#[derive(Default)]
pub struct CreateRevision {
    files: Option<FileFinder>,
}

impl CreateRevision {
    /// The method, whose texts find referenced files with `files`.
    pub fn with_files(files: FileFinder) -> Self {
        Self { files: Some(files) }
    }

    async fn run(&self, request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        check_writes(request)?;
        let (_, diff_phid) = load_diff(request).await?;
        // The method defines no "message": there is no comment.
        let transactions = field_edit_transactions(request, &diff_phid)?;
        let result = apply(request, self.files, None, transactions).await?;
        let id = result["object"]["id"]
            .as_i64()
            .ok_or_else(|| MethodError::Internal("the editor returned no ID".to_owned()))?;
        revision_answer(request, id, &json!(id))
    }
}

impl Method for CreateRevision {
    fn name(&self) -> &'static str {
        "differential.createrevision"
    }

    fn params(&self) -> &'static [&'static str] {
        &["user", "diffid", "fields"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[(ERR_BAD_DIFF, "Bad diff ID.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(self.run(request))
    }
}

/// `differential.updaterevision`.
#[derive(Default)]
pub struct UpdateRevision {
    files: Option<FileFinder>,
}

impl UpdateRevision {
    /// The method, whose texts find referenced files with `files`.
    pub fn with_files(files: FileFinder) -> Self {
        Self { files: Some(files) }
    }

    async fn run(&self, request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        check_writes(request)?;
        let (diff_id, diff_phid) = load_diff(request).await?;
        let id = object_id(request.value("id"))?;
        let revision = load_revision(request, id, true)
            .await?
            .ok_or_else(|| conduit_error(ERR_BAD_REVISION))?;
        if revision.status == "published" {
            return Err(conduit_error(ERR_CLOSED));
        }
        let mut transactions = field_edit_transactions(request, &diff_phid)?;
        let message = match request.value("message") {
            Some(Value::String(message)) => message.as_str(),
            // `strlen(null)` is 0.
            None | Some(Value::Null) => "",
            Some(_) => return Err(MethodError::Unsupported("messages which are not strings")),
        };
        let description = (!message.is_empty()).then(|| diff_description(message));
        if !message.is_empty() {
            transactions.push(json!({"type": "comment", "value": message}));
        }
        let result = apply(request, self.files, Some(revision.id), transactions).await;
        // PHP saves the description before the edit, which may fail: unless
        // the edit is PHP's, the description is saved either way.
        if let Some(description) = description
            && matches!(result, Ok(_) | Err(MethodError::Conduit { .. }))
        {
            save_diff_description(request, diff_id, &description).await?;
        }
        result?;
        revision_answer(request, revision.id, &json!(revision.id.to_string()))
    }
}

impl Method for UpdateRevision {
    fn name(&self) -> &'static str {
        "differential.updaterevision"
    }

    fn params(&self) -> &'static [&'static str] {
        &["id", "diffid", "fields", "message"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[
            (ERR_BAD_DIFF, "Bad diff ID."),
            (ERR_BAD_REVISION, "Bad revision ID."),
            ("ERR_WRONG_USER", "You are not the author of this revision."),
            (ERR_CLOSED, "This revision has already been closed."),
        ]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(self.run(request))
    }
}

/// `differential.close`: the `close` action, on an editor which, like
/// `differential.revision.edit`'s, continues on no effect.
#[derive(Default)]
pub struct Close {
    files: Option<FileFinder>,
}

impl Close {
    /// The method, finding referenced files with `files`.
    pub fn with_files(files: FileFinder) -> Self {
        Self { files: Some(files) }
    }

    async fn run(&self, request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        check_writes(request)?;
        let id = object_id(request.value("revisionID"))?;
        let revision = load_revision(request, id, false)
            .await?
            .ok_or_else(|| conduit_error(ERR_NOT_FOUND))?;
        let transactions = vec![json!({"type": "close", "value": true})];
        apply(request, self.files, Some(revision.id), transactions).await?;
        Ok(Value::Null)
    }
}

impl Method for Close {
    fn name(&self) -> &'static str {
        "differential.close"
    }

    fn params(&self) -> &'static [&'static str] {
        &["revisionID"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[(ERR_NOT_FOUND, "Revision was not found.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(self.run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_like_php() {
        assert_eq!(
            read_field_value(Field::RevisionId, &json!(5)).unwrap(),
            json!("5")
        );
        assert_eq!(
            read_field_value(Field::Title, &json!(3)),
            Err(core_error(
                "Field \"title\" expects a string value, but received a value of type \
                 \"integer\"."
                    .to_owned()
            ))
        );
        assert_eq!(
            read_field_value(Field::Reviewers, &json!(null)),
            Err(core_error(
                "Field \"reviewerPHIDs\" expects a list of strings, but received a value of \
                 type \"NULL\"."
                    .to_owned()
            ))
        );
        assert_eq!(
            field_transactions(Field::Reviewers, json!(["PHID-USER-a", "PHID-PROJ-b!"])).unwrap(),
            [json!({"type": "reviewers.set", "value": ["PHID-USER-a", "blocking(PHID-PROJ-b)"]})]
        );
        assert_eq!(
            field_transactions(Field::BugId, json!("1002")).unwrap(),
            [json!({"type": "bugzilla.bug-id", "value": "1002"})]
        );
        assert_eq!(
            field_transactions(Field::RevisionId, json!("5")).unwrap(),
            Vec::<Value>::new()
        );
    }

    #[test]
    fn describes_diffs_like_php() {
        assert_eq!(diff_description("Fix it\r\n\nMore"), "Fix it");
        assert_eq!(
            diff_description(&"x".repeat(100)),
            format!("{}\u{2026}", "x".repeat(79))
        );
    }
}
