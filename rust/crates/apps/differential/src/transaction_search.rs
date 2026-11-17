//! `transaction.search` for revisions (`TransactionSearchConduitAPIMethod`
//! with `DifferentialTransactionQuery`): the transactions of a revision,
//! or of every revision, with their comments and the Conduit type and
//! fields of their modular type.
//!
//! Objects other than revisions, and object names other than revision
//! PHIDs and monograms, are left to PHP.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, IdOrder};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, edges, rows};
use phab_policy::PolicySubject;
use serde_json::{Map, Value, json};

use crate::edge_search;
use crate::storage::{self, Transaction};

const QUERY_CLASS: &str = "DifferentialTransactionQuery";

/// `DifferentialChangeType::FILE_DIRECTORY`.
const FILE_DIRECTORY: i64 = 4;

/// A `differential_transaction` row.
#[derive(Debug, Clone)]
struct TransactionRow {
    id: i64,
    transaction: Transaction,
    date_created: i64,
    date_modified: i64,
}

impl TransactionRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            transaction: storage::transaction_from_row(row)?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
        })
    }
}

/// A `differential_transaction_comment` row of a transaction.
#[derive(Debug, Clone)]
struct CommentRow {
    id: i64,
    phid: Phid,
    transaction_phid: Phid,
    author_phid: Phid,
    view_policy: String,
    edit_policy: String,
    version: i64,
    content: String,
    is_deleted: bool,
    date_created: i64,
    date_modified: i64,
    changeset_id: Option<i64>,
    is_new_file: bool,
    line_number: i64,
    line_length: i64,
    fixed_state: Option<String>,
    reply_to: Option<String>,
    attributes: Value,
}

impl CommentRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            transaction_phid: Phid::new(rows::text(row, "transactionPHID")?),
            author_phid: Phid::new(rows::text(row, "authorPHID")?),
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            version: rows::int(row, "commentVersion")?,
            content: rows::text(row, "content")?,
            is_deleted: rows::flag(row, "isDeleted")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
            changeset_id: rows::opt_int(row, "changesetID")?,
            is_new_file: rows::flag(row, "isNewFile")?,
            line_number: rows::int(row, "lineNumber")?,
            line_length: rows::int(row, "lineLength")?,
            fixed_state: rows::opt_text(row, "fixedState")?,
            reply_to: rows::opt_text(row, "replyToCommentPHID")?,
            attributes: rows::json(row, "attributes")?,
        })
    }

    fn subject(&self) -> PolicySubject {
        storage::Comment {
            phid: self.phid.clone(),
            author_phid: self.author_phid.clone(),
            view_policy: self.view_policy.clone(),
            edit_policy: self.edit_policy.clone(),
            content: String::new(),
        }
        .subject()
    }

    /// `DifferentialTransactionComment::getSuggestionText()`.
    fn suggestion_text(&self) -> Result<Option<String>, MethodError> {
        let Some(state) = self.attributes.get("inline.state") else {
            return Ok(None);
        };
        if !state.is_object() {
            return Ok(None);
        }
        if !state
            .get("hasSuggestion")
            .is_some_and(phab_core::php::is_truthy)
        {
            return Ok(None);
        }
        match state.get("suggestionText") {
            None | Some(Value::Null) => Ok(Some(String::new())),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(MethodError::Unsupported("an unusual suggestion")),
        }
    }

    /// The comment as `transaction.search` shows it.
    fn result(&self, removed: bool) -> Value {
        let raw = if removed { "" } else { self.content.as_str() };
        json!({
            "id": self.id,
            "phid": self.phid.as_str(),
            "version": self.version,
            "authorPHID": self.author_phid.as_str(),
            "dateCreated": self.date_created,
            "dateModified": self.date_modified,
            "removed": self.is_deleted,
            "content": {"raw": raw},
        })
    }
}

const COMMENT_COLUMNS: &str = "id, phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
     commentVersion, content, isDeleted, dateCreated, dateModified, changesetID, isNewFile, \
     lineNumber, lineLength, fixedState, replyToCommentPHID, attributes";

/// The changeset of an inline comment, with its diff.
#[derive(Debug, Clone)]
struct ChangesetRow {
    filename: String,
    file_type: i64,
    diff_id: i64,
    diff_phid: String,
    revision_id: Option<i64>,
}

/// The object `objectIdentifier` or `objectType` selects
/// (`loadTemplateObject()`): a revision, or every revision.
async fn template_object(request: &ApiRequest<'_>) -> Result<Option<Phid>, MethodError> {
    let identifier = request.value("objectIdentifier").filter(|v| !v.is_null());
    let object_type = request.value("objectType").filter(|v| !v.is_null());
    match (identifier, object_type) {
        (None, None) => Err(pager::core_error(
            "Calls to \"transaction.search\" must specify either an \"objectType\" or an \
             \"objectIdentifier\""
                .to_owned(),
        )),
        (Some(_), Some(_)) => Err(pager::core_error(
            "Calls to \"transaction.search\" must not specify both an \"objectType\" and an \
             \"objectIdentifier\"."
                .to_owned(),
        )),
        (None, Some(Value::String(kind))) if kind == "DREV" => Ok(None),
        (None, Some(_)) => Err(MethodError::Unsupported("transactions of other objects")),
        (Some(Value::String(name)), None) => visible_revision(request, name).await.map(Some),
        (Some(_), None) => Err(MethodError::Unsupported("an unusual object identifier")),
    }
}

/// The revision `name`, a PHID or a monogram, if the viewer can see it.
async fn visible_revision(request: &ApiRequest<'_>, name: &str) -> Result<Phid, MethodError> {
    let database = &request.context.database;
    let revisions = if name.starts_with("PHID-") {
        let phid = Phid::new(name.to_owned());
        if phid.phid_type() != PhidType::REVISION {
            return Err(MethodError::Unsupported("transactions of other objects"));
        }
        storage::load_revisions(database, &[phid]).await?
    } else {
        // DifferentialRevisionPHIDType::canLoadNamedObject().
        let digits = name
            .strip_prefix(['D', 'd'])
            .filter(|digits| {
                !digits.starts_with('0')
                    && !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
            })
            .ok_or(MethodError::Unsupported("transactions of other objects"))?;
        let id: i64 = digits
            .parse()
            .map_err(|_| MethodError::Unsupported("a huge revision ID"))?;
        storage::load_revisions_by_id(database, &[id]).await?
    };
    let visible = storage::visible_revisions(
        database,
        &request.context.config,
        &request.viewer,
        revisions,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    visible
        .into_iter()
        .next()
        .map(|revision| revision.phid)
        .ok_or_else(|| {
            pager::core_error(format!(
                "In call to \"transaction.search\", specified \"objectIdentifier\" (\"{name}\") \
                 does not exist."
            ))
        })
}

/// The `phids` and `authorPHIDs` constraints, `None` when absent.
type TransactionConstraints = (Option<Vec<String>>, Option<Vec<String>>);

/// `applyConstraints()`: the `phids` and `authorPHIDs` constraints.
fn constraints(value: Option<&Value>) -> Result<TransactionConstraints, MethodError> {
    let map = match value {
        None | Some(Value::Null) => return Ok((None, None)),
        Some(Value::Array(list)) if list.is_empty() => return Ok((None, None)),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(MethodError::Unsupported("unusual constraints")),
    };
    if map.keys().any(|key| key != "phids" && key != "authorPHIDs") {
        return Err(MethodError::Unsupported("unknown transaction constraints"));
    }
    let list = |key: &str| -> Result<Option<Vec<String>>, MethodError> {
        let list = match map.get(key) {
            None => return Ok(None),
            Some(Value::Array(list)) => list,
            // PhutilTypeSpec rejects null too.
            Some(_) => return Err(MethodError::Unsupported("constraints which are not lists")),
        };
        if list.is_empty() {
            return Err(pager::core_error(format!(
                "Constraint \"{key}\" to \"transaction.search\" requires nonempty list, empty \
                 list provided."
            )));
        }
        list.iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            ))
    };
    Ok((list("phids")?, list("authorPHIDs")?))
}

/// `DifferentialTransactionQuery` for `transaction.search`.
struct Transactions<'a> {
    request: &'a ApiRequest<'a>,
    object: Option<Phid>,
    phids: Option<Vec<String>>,
    authors: Option<Vec<String>>,
}

impl Transactions<'_> {
    /// `willFilterPage()` drops the transactions of revisions the viewer
    /// can't see; then each transaction follows its own policy.
    async fn visible(&self, rows: Vec<TransactionRow>) -> Result<Vec<TransactionRow>, MethodError> {
        let context = self.request.context;
        let database = &context.database;
        let mut objects: Vec<Phid> = rows
            .iter()
            .map(|row| row.transaction.object_phid.clone())
            .collect();
        objects.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        objects.dedup();
        let revisions = if objects
            .iter()
            .all(|phid| phid.phid_type() == PhidType::REVISION)
        {
            storage::load_revisions(database, &objects).await?
        } else {
            return Err(MethodError::Unsupported("transactions of other objects"));
        };
        let visible: HashSet<Phid> = storage::visible_revisions(
            database,
            &context.config,
            &self.request.viewer,
            revisions,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?
        .into_iter()
        .map(|revision| revision.phid)
        .collect();
        let rows: Vec<TransactionRow> = rows
            .into_iter()
            .filter(|row| visible.contains(&row.transaction.object_phid))
            .collect();
        let subjects: Vec<PolicySubject> =
            rows.iter().map(|row| row.transaction.subject()).collect();
        let allowed = storage::visible_subjects(
            database,
            &context.config,
            &self.request.viewer,
            &subjects,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        Ok(rows
            .into_iter()
            .zip(allowed)
            .filter_map(|(row, allowed)| allowed.then_some(row))
            .collect())
    }
}

impl PolicyAwareQuery for Transactions<'_> {
    type Row = TransactionRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &TransactionRow) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    /// The cursor's revision is loaded under policy, which raises.
    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<TransactionRow>, MethodError> {
        let database = &self.request.context.database;
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        } else {
            if let Some(phids) = &self.phids {
                clauses.in_list("phid", search::strings(phids));
            }
            if let Some(object) = &self.object {
                clauses.in_list("objectPHID", [json!(object.as_str())]);
            }
            if let Some(authors) = &self.authors {
                clauses.in_list("authorPHID", search::strings(authors));
            }
        }
        let (comparison, order) = search::id_paging(IdOrder::Newest, direction);
        if let Some(after) = after {
            clauses.condition(format!("id {comparison} ?"), [json!(after)]);
        }
        let mut tail = format!(" ORDER BY id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(
            &format!(
                "SELECT id, dateModified, {} FROM {}",
                storage::TRANSACTION_COLUMNS,
                database.table("differential", "differential_transaction")
            ),
            &tail,
        );
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(TransactionRow::from_row)
            .collect::<Result<_, _>>()?)
    }

    async fn filter(&self, rows: Vec<TransactionRow>) -> Result<Vec<TransactionRow>, MethodError> {
        self.visible(rows).await
    }
}

/// The visible comments of these transactions, each newest version first
/// (`PhabricatorApplicationTransactionTemplatedCommentQuery`).
async fn load_comments(
    request: &ApiRequest<'_>,
    transactions: &[Phid],
) -> Result<HashMap<Phid, Vec<CommentRow>>, MethodError> {
    let database = &request.context.database;
    let mut map: HashMap<Phid, Vec<CommentRow>> = HashMap::new();
    if transactions.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT {COMMENT_COLUMNS} FROM {} WHERE transactionPHID IN ({}) ORDER BY id DESC",
        database.table("differential", "differential_transaction_comment"),
        phab_db::placeholders(transactions.len())
    );
    let comments: Vec<CommentRow> = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        transactions.iter().map(Phid::as_str),
    )
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(CommentRow::from_row)
    .collect::<Result<_, _>>()?;
    let comments = visible_comments(request, comments).await?;
    // msort() by version, then array_reverse(): the newest version first,
    // and the lowest ID first among equal versions.
    let mut comments = comments;
    comments.sort_by_key(|comment| comment.version);
    comments.reverse();
    for comment in comments {
        map.entry(comment.transaction_phid.clone())
            .or_default()
            .push(comment);
    }
    Ok(map)
}

/// The comments the viewer can see.
async fn visible_comments(
    request: &ApiRequest<'_>,
    comments: Vec<CommentRow>,
) -> Result<Vec<CommentRow>, MethodError> {
    let subjects: Vec<PolicySubject> = comments.iter().map(CommentRow::subject).collect();
    let allowed = storage::visible_subjects(
        &request.context.database,
        &request.context.config,
        &request.viewer,
        &subjects,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    Ok(comments
        .into_iter()
        .zip(allowed)
        .filter_map(|(comment, allowed)| allowed.then_some(comment))
        .collect())
}

/// The visible comments with these PHIDs (`needComments()`), by PHID.
async fn load_attached_comments(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<HashMap<Phid, CommentRow>, MethodError> {
    let database = &request.context.database;
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT {COMMENT_COLUMNS} FROM {} WHERE phid IN ({})",
        database.table("differential", "differential_transaction_comment"),
        phab_db::placeholders(phids.len())
    );
    let comments: Vec<CommentRow> =
        phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(CommentRow::from_row)
            .collect::<Result<_, _>>()?;
    Ok(visible_comments(request, comments)
        .await?
        .into_iter()
        .map(|comment| (comment.phid.clone(), comment))
        .collect())
}

/// The changesets of inline comments, with their diffs
/// (`DifferentialRevisionInlineTransaction::loadTransactionTypeConduitData()`).
async fn load_changesets(
    database: &Database,
    ids: &[i64],
) -> Result<HashMap<i64, ChangesetRow>, MethodError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT c.id, c.filename, c.fileType, d.id AS diffID, d.phid AS diffPHID, d.revisionID \
         FROM {} c JOIN {} d ON d.id = c.diffID WHERE c.id IN ({})",
        database.table("differential", "differential_changeset"),
        database.table("differential", "differential_diff"),
        phab_db::placeholders(ids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
        .fetch_all(database.pool())
        .await?;
    Ok(found
        .iter()
        .map(|row| {
            Ok((
                rows::int(row, "id")?,
                ChangesetRow {
                    filename: rows::text(row, "filename")?,
                    file_type: rows::int(row, "fileType")?,
                    diff_id: rows::int(row, "diffID")?,
                    diff_phid: rows::text(row, "diffPHID")?,
                    revision_id: rows::opt_int(row, "revisionID")?,
                },
            ))
        })
        .collect::<Result<_, DbError>>()?)
}

/// `{"old": ..., "new": ...}`.
fn old_new(transaction: &Transaction) -> Value {
    json!({
        "old": php::round_trip(&transaction.old_value),
        "new": php::round_trip(&transaction.new_value),
    })
}

/// A map of reviewer PHIDs to statuses; PHP decodes `[]` to one too.
fn reviewer_map(value: &Value) -> Result<Vec<(&str, &Value)>, MethodError> {
    match value {
        Value::Object(map) => Ok(map
            .iter()
            .map(|(key, value)| (key.as_str(), value))
            .collect()),
        Value::Array(list) if list.is_empty() => Ok(Vec::new()),
        _ => Err(MethodError::Unsupported("unusual reviewer values")),
    }
}

/// `DifferentialRevisionReviewersTransaction::getFieldValuesForConduit()`:
/// the added reviewers, then the removed ones, then the updated ones.
fn reviewer_operations(transaction: &Transaction) -> Result<Value, MethodError> {
    const BLOCKING: &str = "blocking";
    let old = reviewer_map(&transaction.old_value)?;
    let new = reviewer_map(&transaction.new_value)?;
    let in_old = |phid: &str| old.iter().find(|(key, _)| *key == phid).map(|(_, v)| *v);
    let in_new = |phid: &str| new.iter().find(|(key, _)| *key == phid).map(|(_, v)| *v);
    let mut operations = Vec::new();
    for (phid, value) in &new {
        if in_old(phid).is_none() {
            operations.push(json!({
                "operation": "add",
                "phid": phid,
                "oldStatus": null,
                "newStatus": php::round_trip(value),
                "isBlocking": value.as_str() == Some(BLOCKING),
            }));
        }
    }
    for (phid, value) in &old {
        if in_new(phid).is_none() {
            operations.push(json!({
                "operation": "remove",
                "phid": phid,
                "oldStatus": php::round_trip(value),
                "newStatus": null,
                "isBlocking": false,
            }));
        }
    }
    for (phid, old_status) in &old {
        let Some(new_status) = in_new(phid) else {
            continue;
        };
        if old_status == &new_status {
            continue;
        }
        if !old_status.is_string() || !new_status.is_string() {
            return Err(MethodError::Unsupported("unusual reviewer statuses"));
        }
        operations.push(json!({
            "operation": "update",
            "phid": phid,
            "oldStatus": old_status,
            "newStatus": new_status,
            "isBlocking": new_status.as_str() == Some(BLOCKING),
        }));
    }
    Ok(json!({"operations": operations}))
}

/// `PhabricatorEdgeChangeRecord::getPHIDsFromTransactionValue()`.
fn edge_destinations(value: &Value) -> Result<Vec<&str>, MethodError> {
    let unusual = || MethodError::Unsupported("unusual edge values");
    let items: Vec<&Value> = match value {
        Value::Null => return Ok(Vec::new()),
        Value::Array(list) => list.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => return Err(unusual()),
    };
    let Some(head) = items.first() else {
        return Ok(Vec::new());
    };
    if head.is_object() {
        items
            .iter()
            .map(|item| item.get("dst").and_then(Value::as_str))
            .collect::<Option<_>>()
            .ok_or_else(unusual)
    } else {
        items
            .iter()
            .map(|item| item.as_str())
            .collect::<Option<_>>()
            .ok_or_else(unusual)
    }
}

/// `newEdgeTransactionFields()`: the added PHIDs, then the removed ones.
fn edge_operations(transaction: &Transaction) -> Result<Value, MethodError> {
    let old = edge_destinations(&transaction.old_value)?;
    let new = edge_destinations(&transaction.new_value)?;
    let mut operations = Vec::new();
    let mut seen = HashSet::new();
    for phid in &new {
        if !old.contains(phid) && seen.insert(*phid) {
            operations.push(json!({"operation": "add", "phid": phid}));
        }
    }
    let mut seen = HashSet::new();
    for phid in &old {
        if !new.contains(phid) && seen.insert(*phid) {
            operations.push(json!({"operation": "remove", "phid": phid}));
        }
    }
    Ok(json!({"operations": operations}))
}

/// The fields of an inline comment
/// (`DifferentialRevisionInlineTransaction::getFieldValuesForConduit()`).
fn inline_fields(
    comment: &CommentRow,
    changesets: &HashMap<i64, ChangesetRow>,
) -> Result<Value, MethodError> {
    let changeset = comment
        .changeset_id
        .and_then(|id| changesets.get(&id))
        .ok_or(MethodError::Unsupported(
            "an inline on a changeset the viewer can't see",
        ))?;
    let is_done = matches!(comment.fixed_state.as_deref(), Some("done" | "undraft"));
    let suggestion = comment.suggestion_text()?;
    let has_suggestion = suggestion.is_some();
    let suggestion = suggestion.filter(|_| !comment.is_deleted);
    let mut path = changeset.filename.clone();
    if changeset.file_type == FILE_DIRECTORY {
        path.push('/');
    }
    Ok(json!({
        "diff": {"id": changeset.diff_id, "phid": changeset.diff_phid},
        "path": path,
        "line": comment.line_number,
        "length": comment.line_length + 1,
        "isNewFile": comment.is_new_file,
        "replyToCommentPHID": comment.reply_to,
        "isDone": is_done,
        "hasSuggestion": has_suggestion,
        "suggestionText": suggestion,
    }))
}

/// The modular type of a transaction for Conduit, and its fields
/// (`getTransactionTypeForConduit()`, `getFieldValuesForConduit()`), then
/// the fallback for core types.
fn type_and_fields(
    transaction: &Transaction,
    comment: Option<&CommentRow>,
    changesets: &HashMap<i64, ChangesetRow>,
) -> Result<(Option<&'static str>, Value), MethodError> {
    let empty = || Value::Object(Map::new());
    let action = |name| Ok((Some(name), empty()));
    match transaction.transaction_type.as_str() {
        "differential.revision.abandon" => action("abandon"),
        "differential.revision.accept" => action("accept"),
        "differential.revision.commandeer" => action("commandeer"),
        "differential.revision.plan" => action("plan-changes"),
        "differential.revision.reclaim" => action("reclaim"),
        "differential.revision.reject" => action("request-changes"),
        "differential.revision.reopen" => action("reopen"),
        "differential.revision.request" => action("request-review"),
        "differential.revision.resign" => action("resign"),
        "differential.revision.author" => Ok((Some("author"), old_new(transaction))),
        "draft" => Ok((Some("draft"), old_new(transaction))),
        "differential.revision.status" => Ok((Some("status"), old_new(transaction))),
        "differential.revision.summary" => Ok((Some("summary"), old_new(transaction))),
        "differential.revision.testplan" => Ok((Some("testPlan"), old_new(transaction))),
        "differential.revision.title" => Ok((Some("title"), old_new(transaction))),
        "differential.revision.close" => {
            let commit = transaction
                .metadata_value("commitPHID")
                .filter(|value| phab_core::php::is_truthy(value));
            let commits: Vec<Value> = commit.into_iter().map(php::round_trip).collect();
            Ok((Some("close"), json!({"commitPHIDs": commits})))
        }
        "differential:update" => {
            let commits = transaction
                .metadata_value("commitPHIDs")
                .filter(|value| !value.is_null())
                .map_or_else(|| json!([]), php::round_trip);
            let mut fields = old_new(transaction);
            fields["commitPHIDs"] = commits;
            Ok((Some("update"), fields))
        }
        "differential.revision.reviewers" => {
            Ok((Some("reviewers"), reviewer_operations(transaction)?))
        }
        "differential:inline" => {
            let comment =
                comment.ok_or(MethodError::Unsupported("an inline the viewer can't see"))?;
            Ok((Some("inline"), inline_fields(comment, changesets)?))
        }
        "core:comment" => Ok((Some("comment"), empty())),
        "core:create" => Ok((Some("create"), empty())),
        "core:subscribers" => Ok((Some("subscribers"), edge_operations(transaction)?)),
        "core:edge" => {
            let edge_type = transaction.metadata_value("edge:type");
            let is_project = match edge_type {
                Some(Value::Number(number)) => number.as_i64() == Some(edges::OBJECT_HAS_PROJECT),
                Some(Value::String(text)) => text == &edges::OBJECT_HAS_PROJECT.to_string(),
                _ => false,
            };
            if is_project {
                Ok((Some("projects"), edge_operations(transaction)?))
            } else {
                Ok((None, empty()))
            }
        }
        _ => Ok((None, empty())),
    }
}

/// `getTransactionGroupID()`: the `core.groupID` metadata, if a nonempty
/// string.
fn group_id(transaction: &Transaction) -> Result<Option<String>, MethodError> {
    match transaction.metadata_value("core.groupID") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(MethodError::Unsupported("an unusual transaction group")),
    }
}

/// The page of transactions as `transaction.search` shows them.
async fn results(
    request: &ApiRequest<'_>,
    rows: &[TransactionRow],
) -> Result<Vec<Value>, MethodError> {
    let database = &request.context.database;
    let phids: Vec<Phid> = rows
        .iter()
        .map(|row| row.transaction.phid.clone())
        .collect();
    let comments = load_comments(request, &phids).await?;
    let attached: Vec<Phid> = rows
        .iter()
        .filter(|row| row.transaction.transaction_type == "differential:inline")
        .filter_map(|row| row.transaction.comment_phid.clone())
        .collect();
    let attached = load_attached_comments(request, &attached).await?;
    let mut changeset_ids: Vec<i64> = attached
        .values()
        .filter_map(|comment| comment.changeset_id)
        .collect();
    changeset_ids.sort_unstable();
    changeset_ids.dedup();
    // The changesets of diffs of other revisions follow those revisions'
    // policies.
    let object_ids: HashSet<i64> = {
        let objects: Vec<Phid> = rows
            .iter()
            .map(|row| row.transaction.object_phid.clone())
            .collect();
        storage::load_revisions(database, &objects)
            .await?
            .iter()
            .map(|revision| revision.id)
            .collect()
    };
    let changesets: HashMap<i64, ChangesetRow> = load_changesets(database, &changeset_ids)
        .await?
        .into_iter()
        .filter(|(_, changeset)| {
            changeset
                .revision_id
                .is_some_and(|id| object_ids.contains(&id))
        })
        .collect();
    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        let transaction = &row.transaction;
        let comment_data: Vec<Value> = match comments.get(&transaction.phid) {
            Some(versions) => {
                let removed = versions.first().is_some_and(|newest| newest.is_deleted);
                versions
                    .iter()
                    .map(|comment| comment.result(removed))
                    .collect()
            }
            None => Vec::new(),
        };
        let comment = transaction
            .comment_phid
            .as_ref()
            .and_then(|phid| attached.get(phid));
        let (kind, fields) = type_and_fields(transaction, comment, &changesets)?;
        data.push(json!({
            "id": row.id,
            "phid": transaction.phid.as_str(),
            "type": kind,
            "authorPHID": transaction.author_phid.as_str(),
            "objectPHID": transaction.object_phid.as_str(),
            "dateCreated": row.date_created,
            "dateModified": row.date_modified,
            "groupID": group_id(transaction)?,
            "comments": comment_data,
            "fields": fields,
        }));
    }
    Ok(data)
}

/// `transaction.search`.
pub struct TransactionSearch;

impl Method for TransactionSearch {
    fn name(&self) -> &'static str {
        "transaction.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "objectIdentifier",
            "objectType",
            "constraints",
            "before",
            "after",
            "limit",
        ]
    }

    /// An infrastructure method, of no application.
    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let page_size = edge_search::limit(request.value("limit"))?;
            let paging = pager::Paging {
                size: pager::PageSize {
                    shown: json!(page_size),
                    value: page_size,
                },
                after: pager::Cursor::parse(request.value("after"))?,
                before: pager::Cursor::parse(request.value("before"))?,
            };
            let object = template_object(request).await?;
            let (phids, authors) = constraints(request.value("constraints"))?;
            let query = Transactions {
                request,
                object,
                phids,
                authors,
            };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, page_size + 1).await?;
            let page = paging.pager().slice(found);
            let data = results(request, &page.items).await?;
            let cursor = |row: &TransactionRow| row.id.to_string();
            Ok(json!({
                "data": data,
                "cursor": {
                    "limit": page_size,
                    "after": page.next.as_ref().map(cursor),
                    "before": page.previous.as_ref().map(cursor),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction(kind: &str, old: Value, new: Value) -> Transaction {
        Transaction {
            phid: Phid::new("PHID-XACT-DREV-a".to_owned()),
            author_phid: Phid::new("PHID-USER-a".to_owned()),
            object_phid: Phid::new("PHID-DREV-a".to_owned()),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            comment_phid: None,
            transaction_type: kind.to_owned(),
            old_value: old,
            new_value: new,
            metadata: json!({}),
        }
    }

    #[test]
    fn reviewer_operations_like_php() {
        let xaction = transaction(
            "differential.revision.reviewers",
            json!({"PHID-USER-b": "added", "PHID-USER-c": "added", "PHID-USER-d": "accepted"}),
            json!({"PHID-USER-e": "blocking", "PHID-USER-b": "blocking", "PHID-USER-d": "accepted"}),
        );
        assert_eq!(
            reviewer_operations(&xaction),
            Ok(json!({"operations": [
                {"operation": "add", "phid": "PHID-USER-e", "oldStatus": null,
                 "newStatus": "blocking", "isBlocking": true},
                {"operation": "remove", "phid": "PHID-USER-c", "oldStatus": "added",
                 "newStatus": null, "isBlocking": false},
                {"operation": "update", "phid": "PHID-USER-b", "oldStatus": "added",
                 "newStatus": "blocking", "isBlocking": true},
            ]}))
        );
        let created = transaction(
            "differential.revision.reviewers",
            json!([]),
            json!({"PHID-USER-b": "added"}),
        );
        assert_eq!(
            reviewer_operations(&created)
                .map(|fields| fields["operations"].as_array().map(Vec::len)),
            Ok(Some(1))
        );
    }

    #[test]
    fn edge_operations_of_lists_and_maps() {
        let lists = transaction(
            "core:subscribers",
            json!(["PHID-USER-a"]),
            json!(["PHID-USER-b"]),
        );
        assert_eq!(
            edge_operations(&lists),
            Ok(json!({"operations": [
                {"operation": "add", "phid": "PHID-USER-b"},
                {"operation": "remove", "phid": "PHID-USER-a"},
            ]}))
        );
        let maps = transaction(
            "core:edge",
            json!([]),
            json!({"PHID-PROJ-a": {"src": "PHID-DREV-a", "type": 41, "dst": "PHID-PROJ-a"}}),
        );
        assert_eq!(
            edge_operations(&maps),
            Ok(json!({"operations": [{"operation": "add", "phid": "PHID-PROJ-a"}]}))
        );
    }

    #[test]
    fn constraints_like_php() {
        assert_eq!(constraints(None), Ok((None, None)));
        assert_eq!(
            constraints(Some(&json!({"authorPHIDs": ["PHID-USER-a"]}))),
            Ok((None, Some(vec!["PHID-USER-a".to_owned()])))
        );
        assert!(matches!(
            constraints(Some(&json!({"phids": []}))),
            Err(MethodError::Conduit { .. })
        ));
        assert!(matches!(
            constraints(Some(&json!({"bogus": []}))),
            Err(MethodError::Unsupported(_))
        ));
    }
}
