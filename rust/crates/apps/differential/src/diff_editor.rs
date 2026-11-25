//! `DifferentialDiffEditor` creating a diff (`differential:diff:create`):
//! what `differential.creatediff` and `differential.createrawdiff` share.
//!
//! PHP builds the changesets (`DifferentialDiff::newFromRawChanges()`,
//! ported in [`phab_diff::build`]), runs Herald's diff rules as a
//! validation, then saves the diff, its changesets and hunks, the
//! transaction, and queues the publish task, which has nothing to publish
//! for diffs (no mail, feed or search). Here everything is checked first,
//! and one MySQL transaction writes the same rows.
//!
//! Left to PHP: Herald rules on diffs (they may reject the diff), binary
//! files which are thumbnails of other files, `differential.generated-paths`
//! patterns which don't compile, values of a type or length MySQL would
//! refuse or convert, and repositories which exist but which the viewer
//! can't see (PHP raises a policy exception).

use std::collections::HashMap;

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, PhidType, php, random};
use phab_db::sqlx::MySqlConnection;
use phab_db::{rows, worker};
use phab_diff::build::{self, FileObject, NewChangeset};
use serde_json::{Map, Value, json};

/// `DifferentialChangesetPHIDType`.
const CHANGESET: PhidType = PhidType::new(*b"DCNG");

/// The content source of a Conduit call (`PhabricatorConduitContentSource`).
const CONDUIT_SOURCE: &str = phab_db::editor::CONDUIT_SOURCE;

/// `DifferentialLintStatus::LINT_SKIP` and `DifferentialUnitStatus::UNIT_SKIP`.
pub(crate) const STATUS_SKIP: i64 = 4;

/// The longest value of a `text255` column.
const TEXT255: usize = 255;

/// The diff's columns from the `TYPE_DIFF_CREATE` dictionary
/// (`updateDiffFromDict()`).
#[derive(Debug, Clone, Default)]
pub(crate) struct DiffFields {
    pub(crate) source_path: Option<String>,
    pub(crate) source_machine: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) creation_method: Option<String>,
    pub(crate) author_phid: String,
    pub(crate) bookmark: Option<String>,
    pub(crate) repository_phid: Option<String>,
    pub(crate) repository_uuid: Option<String>,
    pub(crate) source_control_system: Option<String>,
    pub(crate) source_control_path: Option<String>,
    pub(crate) source_control_base_revision: Option<String>,
    pub(crate) lint_status: i64,
    pub(crate) unit_status: i64,
}

/// A planned diff: its changesets and fields, the view policy of a
/// `TYPE_VIEW_POLICY` transaction, and the properties
/// `differential.creatediff` saves after it, already encoded.
#[derive(Debug, Clone)]
pub(crate) struct PlannedDiff {
    pub(crate) changesets: Vec<NewChangeset>,
    pub(crate) line_count: i64,
    pub(crate) fields: DiffFields,
    pub(crate) view_policy: Option<String>,
    pub(crate) properties: Vec<(String, String)>,
}

/// The acting user of a call which writes. In shadow mode and on read-only
/// installs writes are skipped, so the call is PHP's alone.
pub(crate) fn actor_phid(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    if request.writes != phab_auth::Writes::Allowed {
        return Err(MethodError::Unsupported("writes when they are skipped"));
    }
    request
        .viewer
        .phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned())
        .ok_or(MethodError::Unsupported("diffs of the omnipotent user"))
}

/// A string parameter as Lisk stores it in a `text255?` column: strings
/// MySQL keeps as they are, or null.
pub(crate) fn text_value(value: Option<&Value>) -> Result<Option<String>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.chars().count() <= TEXT255 => Ok(Some(text.clone())),
        Some(Value::String(_)) => Err(MethodError::Unsupported("values longer than their column")),
        Some(_) => Err(MethodError::Unsupported("values which are not strings")),
    }
}

/// A value as Lisk's `phutil_json_encode()` stores it. Floats are left to
/// PHP, which prints them its own way.
pub(crate) fn encode(value: &Value) -> Result<String, MethodError> {
    fn has_float(value: &Value) -> bool {
        match value {
            Value::Number(number) => !number.is_i64() && !number.is_u64(),
            Value::Array(items) => items.iter().any(has_float),
            Value::Object(entries) => entries.values().any(has_float),
            _ => false,
        }
    }
    fn has_big_integer(value: &Value) -> bool {
        match value {
            Value::Number(number) => number.is_u64() && !number.is_i64(),
            Value::Array(items) => items.iter().any(has_big_integer),
            Value::Object(entries) => entries.values().any(has_big_integer),
            _ => false,
        }
    }
    if has_float(value) || has_big_integer(value) {
        return Err(MethodError::Unsupported("values with floats"));
    }
    Ok(php::json_encode(&php::round_trip(value)))
}

fn encode_metadata(metadata: Option<&Map<String, Value>>) -> Result<String, MethodError> {
    encode(&metadata.cloned().map_or(Value::Null, Value::Object))
}

fn unsupported(error: build::Unsupported) -> MethodError {
    MethodError::Unsupported(error.0)
}

/// `differential.generated-paths`, compiled.
fn generated_paths(request: &ApiRequest<'_>) -> Result<Vec<pcre2::bytes::Regex>, MethodError> {
    let patterns = match request.context.config.value("differential.generated-paths") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(patterns)) => patterns,
        Some(_) => {
            return Err(MethodError::Unsupported(
                "generated paths which are not a list",
            ));
        }
    };
    patterns
        .iter()
        .map(|pattern| {
            pattern
                .as_str()
                .and_then(|pattern| phab_remarkup::preg::compile_php(pattern.as_bytes()))
                .ok_or(MethodError::Unsupported("generated paths PHP must compile"))
        })
        .collect()
}

/// The new files of binary changesets, as `PhabricatorFileQuery` loads
/// them for the omnipotent viewer.
async fn new_files(
    request: &ApiRequest<'_>,
    changesets: &[NewChangeset],
) -> Result<HashMap<String, FileObject>, MethodError> {
    let mut phids = Vec::new();
    for changeset in changesets {
        if let Some(phid) = changeset.new_file_phid().map_err(unsupported)? {
            phids.push(phid.to_owned());
        }
    }
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT transformedPHID FROM {} WHERE transformedPHID IN ({})",
        database.table("file", "file_transformedfile"),
        phab_db::placeholders(phids.len())
    );
    let transforms = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter())
        .fetch_all(database.pool())
        .await?;
    if !transforms.is_empty() {
        return Err(MethodError::Unsupported(
            "binary files which are transforms of other files",
        ));
    }
    let sql = format!(
        "SELECT phid, contentHash FROM {} WHERE phid IN ({})",
        database.table("file", "file"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter())
        .fetch_all(database.pool())
        .await?;
    let mut files = HashMap::new();
    for row in &found {
        files.insert(
            rows::text(row, "phid")?,
            FileObject {
                content_hash: rows::opt_text(row, "contentHash")?,
            },
        );
    }
    Ok(files)
}

/// `newFromRawChanges()`: the changesets as PHP builds them, with the
/// checks of the columns they go to.
pub(crate) async fn build_changesets(
    request: &ApiRequest<'_>,
    changes: &[phab_diff::arcanist::RawChange],
) -> Result<(Vec<NewChangeset>, i64), MethodError> {
    let (mut changesets, line_count) = build::changesets(changes).map_err(unsupported)?;
    let generated = generated_paths(request)?;
    let files = new_files(request, &changesets).await?;
    build::rebuild(&mut changesets, &generated, &files).map_err(unsupported)?;
    let fits = |value: i64| u32::try_from(value).is_ok();
    let columns_fit = fits(line_count)
        && changesets.iter().all(|changeset| {
            fits(changeset.add_lines)
                && fits(changeset.del_lines)
                && fits(changeset.change_type)
                && fits(changeset.file_type)
                && changeset.hunks.iter().all(|hunk| {
                    fits(hunk.old_offset)
                        && fits(hunk.old_len)
                        && fits(hunk.new_offset)
                        && fits(hunk.new_len)
                })
        });
    if !columns_fit {
        return Err(MethodError::Unsupported(
            "numbers an unsigned 32-bit column refuses",
        ));
    }
    Ok((changesets, line_count))
}

/// The Herald rules of `HeraldDifferentialDiffAdapter`, which run as a
/// validation and may reject the diff: any is left to PHP.
pub(crate) async fn check_herald(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE contentType = 'differential.diff' AND isDisabled = 0 LIMIT 1",
        database.table("herald", "herald_rule")
    );
    if phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(MethodError::Unsupported("Herald rules on diffs"));
    }
    Ok(())
}

/// `PhabricatorRepositoryQuery::withPHIDs(array($phid))->executeOne()`:
/// the repository's PHID and UUID, `None` if there is no such repository.
/// One the viewer can't see raises a policy exception in PHP.
pub(crate) async fn repository(
    request: &ApiRequest<'_>,
    phid: &str,
) -> Result<Option<(String, Option<String>)>, MethodError> {
    let database = &request.context.database;
    let found = crate::repository::load(database, &[Phid::new(phid)]).await?;
    if found.is_empty() {
        return Ok(None);
    }
    let visible = crate::repository::visible(
        database,
        &request.context.config,
        &request.viewer,
        found,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    let Some(repository) = visible.into_iter().next() else {
        return Err(MethodError::Unsupported(
            "repositories the viewer can't see",
        ));
    };
    let sql = format!(
        "SELECT uuid FROM {} WHERE phid = ?",
        database.table("repository", "repository")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(repository.phid.as_str())
        .fetch_one(database.pool())
        .await?;
    Ok(Some((
        repository.phid.as_str().to_owned(),
        rows::opt_text(&row, "uuid")?,
    )))
}

/// The publish task's state (`getWorkerState()`): diffs send no mail and
/// publish no feed story.
fn worker_state() -> Value {
    json!({
        "parentMessageID": null,
        "isNewObject": true,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": null,
        "mailToPHIDs": [],
        "mailCCPHIDs": [],
        "feedNotifyPHIDs": [],
        "feedRelatedPHIDs": [],
        "feedShouldPublish": false,
        "mailShouldSend": false,
        "mustEncrypt": [],
        "mailStamps": [],
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": [],
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": [],
        "custom.encoding": [],
    })
}

/// A changeset with its JSON columns encoded.
struct ChangesetRow<'a> {
    changeset: &'a NewChangeset,
    metadata: String,
    old_properties: String,
    new_properties: String,
    away_paths: String,
}

impl<'a> ChangesetRow<'a> {
    fn new(changeset: &'a NewChangeset) -> Result<Self, MethodError> {
        Ok(Self {
            changeset,
            metadata: encode_metadata(changeset.metadata.as_ref())?,
            old_properties: encode(&changeset.old_properties)?,
            new_properties: encode(&changeset.new_properties)?,
            away_paths: encode(&changeset.away_paths)?,
        })
    }

    /// `DifferentialChangeset::save()`: the changeset, then its hunks.
    async fn insert(
        &self,
        request: &ApiRequest<'_>,
        connection: &mut MySqlConnection,
        diff_id: i64,
    ) -> Result<(), MethodError> {
        let database = &request.context.database;
        let now = request.now;
        let changeset = self.changeset;
        let changeset_id = phab_db::sqlx::query(&format!(
            "INSERT INTO {} (diffID, oldFile, filename, awayPaths, changeType, fileType, \
             metadata, oldProperties, newProperties, addLines, delLines, dateCreated, \
             dateModified, phid) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            database.table("differential", "differential_changeset")
        ))
        .bind(diff_id)
        .bind(changeset.old_file.as_deref().map(str::as_bytes))
        .bind(changeset.filename.as_bytes())
        .bind(&self.away_paths)
        .bind(changeset.change_type)
        .bind(changeset.file_type)
        .bind(&self.metadata)
        .bind(&self.old_properties)
        .bind(&self.new_properties)
        .bind(changeset.add_lines)
        .bind(changeset.del_lines)
        .bind(now)
        .bind(now)
        .bind(Phid::generate(CHANGESET, None).into_string())
        .execute(&mut *connection)
        .await?
        .last_insert_id();
        for hunk in &changeset.hunks {
            // `setChanges()`: UTF-8 text, deflated when that saves half.
            let raw = hunk.changes.as_bytes();
            let (format, data) = match phab_diff::deflate::maybe_deflate(raw) {
                Some(deflated) => ("gzde", deflated),
                None => ("byte", raw.to_vec()),
            };
            phab_db::sqlx::query(&format!(
                "INSERT INTO {} (changesetID, oldOffset, oldLen, newOffset, newLen, dataType, \
                 dataEncoding, dataFormat, data, dateCreated, dateModified) VALUES \
                 (?, ?, ?, ?, ?, 'text', 'utf8', ?, ?, ?, ?)",
                database.table("differential", "differential_hunk")
            ))
            .bind(changeset_id)
            .bind(hunk.old_offset)
            .bind(hunk.old_len)
            .bind(hunk.new_offset)
            .bind(hunk.new_len)
            .bind(format)
            .bind(data)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
        }
        Ok(())
    }
}

/// The `differential_diff` row (`updateDiffFromDict()`, then Lisk's
/// insert). Returns its ID.
async fn insert_diff(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    planned: &PlannedDiff,
    phid: &str,
) -> Result<i64, MethodError> {
    let database = &request.context.database;
    let fields = &planned.fields;
    let id = phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, revisionID, authorPHID, repositoryPHID, commitPHID, \
         sourceMachine, sourcePath, sourceControlSystem, sourceControlBaseRevision, \
         sourceControlPath, lintStatus, unitStatus, lineCount, branch, bookmark, \
         creationMethod, repositoryUUID, description, viewPolicy, dateCreated, dateModified) \
         VALUES (?, NULL, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?)",
        database.table("differential", "differential_diff")
    ))
    .bind(phid)
    .bind(&fields.author_phid)
    .bind(&fields.repository_phid)
    .bind(&fields.source_machine)
    .bind(&fields.source_path)
    .bind(&fields.source_control_system)
    .bind(&fields.source_control_base_revision)
    .bind(&fields.source_control_path)
    .bind(fields.lint_status)
    .bind(fields.unit_status)
    .bind(planned.line_count)
    .bind(&fields.branch)
    .bind(&fields.bookmark)
    .bind(&fields.creation_method)
    .bind(&fields.repository_uuid)
    // `initializeNewDiff()`: the author's own, unless the edit sets it.
    .bind(planned.view_policy.as_ref().unwrap_or(&fields.author_phid))
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    i64::try_from(id).map_err(MethodError::internal)
}

/// The `differential:diff:create` transaction, the `core:view-policy` one
/// of `view_policy`, and the publish task.
async fn publish(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    (actor, view_policy): (&str, Option<&str>),
    diff_phid: &str,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let metadata = php::json_encode(&json!({"core.groupID": random::random_characters(32)}));
    // The dictionary is consumed by the editor, which records `true`; the
    // old policy of a new object is `null`.
    let mut rows = vec![("differential:diff:create", "true".to_owned())];
    if let Some(policy) = view_policy {
        rows.push(("core:view-policy", php::json_encode(&json!(policy))));
    }
    let mut xaction_phids = Vec::new();
    for (kind, new) in &rows {
        let xaction_phid =
            Phid::generate(PhidType::TRANSACTION, Some(PhidType::DIFF)).into_string();
        phab_db::editor::insert_transaction(
            connection,
            &database.table("differential", "differential_difftransaction"),
            phab_db::editor::TransactionRow {
                phid: &xaction_phid,
                author: actor,
                object: diff_phid,
                kind,
                old: "null",
                new,
                metadata: &metadata,
                content_source: CONDUIT_SOURCE,
            },
            request.now,
        )
        .await?;
        xaction_phids.push(xaction_phid);
    }
    let data = php::json_encode(&json!({
        "objectPHID": diff_phid,
        "actorPHID": actor,
        "xactionPHIDs": xaction_phids,
        "state": worker_state(),
    }));
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "PhabricatorApplicationTransactionPublishWorker",
            data: &data,
            priority: worker::PRIORITY_ALERTS,
            object_phid: Some(diff_phid),
        },
        request.now,
    )
    .await?;
    Ok(())
}

/// Saves the diff as `applyTransactions()` does, in one MySQL transaction:
/// the diff, its changesets and their hunks (`DifferentialDiff::save()`),
/// the transaction, the publish task, then the properties
/// `differential.creatediff` adds. Returns the diff's ID and PHID.
pub(crate) async fn save(
    request: &ApiRequest<'_>,
    planned: &PlannedDiff,
) -> Result<(i64, String), MethodError> {
    // Everything which may be refused is encoded before writing.
    let changesets = planned
        .changesets
        .iter()
        .map(ChangesetRow::new)
        .collect::<Result<Vec<_>, _>>()?;
    let database = &request.context.database;
    let diff_phid = Phid::generate(PhidType::DIFF, None).into_string();
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    let diff_id = insert_diff(request, connection, planned, &diff_phid).await?;
    for changeset in &changesets {
        changeset.insert(request, connection, diff_id).await?;
    }
    let view_policy = planned.view_policy.as_deref();
    publish(
        request,
        connection,
        (&planned.fields.author_phid, view_policy),
        &diff_phid,
    )
    .await?;
    for (name, data) in &planned.properties {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (diffID, name, data, dateCreated, dateModified) \
             VALUES (?, ?, ?, ?, ?)",
            database.table("differential", "differential_diffproperty")
        ))
        .bind(diff_id)
        .bind(name)
        .bind(data)
        .bind(request.now)
        .bind(request.now)
        .execute(&mut *connection)
        .await?;
    }
    transaction.commit().await?;
    Ok((diff_id, diff_phid))
}
