//! The implicit `core:file` transaction (`newFileTransaction()`): the files
//! the new remarkup references or embeds, which the revision then
//! references (`PhabricatorFileAttachment::MODE_REFERENCE`).

use std::future::Future;
use std::pin::Pin;

use phab_conduit::{ApiRequest, MethodError};
use phab_core::php;
use serde_json::{Map, Value, json};

use super::Plan;

/// What [`FileFinder`] returns.
pub type FilesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<(i64, String)>, MethodError>> + Send + 'a>>;

/// `PhabricatorEmbedFileRemarkupRule::loadObjects()`: the IDs and PHIDs of
/// the files with these IDs the actor can see. The files application
/// provides it, as it depends on this one.
pub type FileFinder = for<'a> fn(&'a ApiRequest<'a>, Vec<i64>) -> FilesFuture<'a>;

/// `PhabricatorFileAttachment::MODE_REFERENCE`.
const MODE_REFERENCE: &str = "reference";

/// The planned transaction: its PHID and the files it references.
pub(super) struct Planned {
    pub(super) phid: String,
    pub(super) added: Vec<String>,
}

/// The files of `texts` with an intent to attach (outside quotes) the
/// actor can see, in order (`extractFilePHIDsFromEmbeddedFiles()`).
async fn referenced(
    request: &ApiRequest<'_>,
    finder: Option<FileFinder>,
    texts: &[&str],
) -> Result<Vec<String>, MethodError> {
    let engine =
        phab_remarkup::phabricator::engine(crate::markup::engine_config(&request.context.config));
    let mut phids: Vec<String> = Vec::new();
    for text in texts {
        let ids: Vec<i64> = engine
            .prepare(text.as_bytes(), phab_remarkup::Mode::Html)
            .file_attach_intent_ids()
            .iter()
            .filter_map(|id| std::str::from_utf8(id).ok()?.parse().ok())
            .collect();
        if ids.is_empty() {
            continue;
        }
        let Some(finder) = finder else {
            return Err(MethodError::Unsupported("references to files"));
        };
        let found = finder(request, ids.clone()).await?;
        for id in ids {
            for (_, phid) in found.iter().filter(|(found, _)| *found == id) {
                if !phids.contains(phid) {
                    phids.push(phid.clone());
                }
            }
        }
    }
    Ok(phids)
}

/// The files the new texts reference which the old ones didn't. Files the
/// revision has already are left to PHP.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    finder: Option<FileFinder>,
    revision_phid: &str,
    (new, old): (&[&str], Option<&str>),
) -> Result<Option<Planned>, MethodError> {
    let mut added = referenced(request, finder, new).await?;
    if let Some(old) = old {
        let before = referenced(request, finder, &[old]).await?;
        added.retain(|phid| !before.contains(phid));
    }
    if added.is_empty() {
        return Ok(None);
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE objectPHID = ? AND filePHID IN ({}) LIMIT 1",
        database.table("file", "file_attachment"),
        phab_db::placeholders(added.len())
    );
    let query = phab_db::sqlx::query(&sql).bind(revision_phid);
    if phab_db::bind_all(query, added.iter().map(String::as_str))
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(MethodError::Unsupported("files the revision has already"));
    }
    Ok(Some(Planned {
        phid: super::transaction_phid(),
        added,
    }))
}

/// The transaction (`newFileTransactionInternalValues()`: the changed
/// attachments before and after) and the attachments.
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let new: Map<String, Value> = planned
        .added
        .iter()
        .map(|phid| (phid.clone(), json!(MODE_REFERENCE)))
        .collect();
    let new = php::json_encode(&Value::Object(new));
    let metadata =
        php::json_encode(&json!({"attach.implicit": true, "core.groupID": plan.group_id}));
    let row = (planned.phid.as_str(), "core:file", "[]", &*new, &*metadata);
    super::insert_transaction(request, plan, connection, row).await?;
    let database = &request.context.database;
    for file in &planned.added {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (objectPHID, filePHID, attachmentMode, attacherPHID, dateCreated, \
             dateModified) VALUES (?, ?, ?, ?, ?, ?)",
            database.table("file", "file_attachment")
        ))
        .bind(plan.revision.phid.as_str())
        .bind(file)
        .bind(MODE_REFERENCE)
        .bind(&plan.actor)
        .bind(request.now)
        .bind(request.now)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}
