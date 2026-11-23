//! Merge conflict rechecks (moz-extensions' `RevisionMergeConflictWorker`):
//! an edit which changes whether or where a revision lands (abandoning it,
//! or re-wiring its stack) queues a check of the revision and of everything
//! stacked above it, when its repository is checked
//! (`shouldCheckMergeConflicts()`, `queueMergeConflictChecks()`).

use phab_conduit::{ApiRequest, MethodError};
use phab_core::php;
use phab_db::sqlx::MySqlConnection;
use phab_db::{edges, rows, worker};
use serde_json::{Value, json};

use crate::repository;
use crate::storage::Revision;

/// `PhabricatorWorker::PRIORITY_BULK`.
const PRIORITY_BULK: i64 = 3000;

/// `RevisionMergeConflictWorker::isEnabledForRepository()` of the
/// revision's repository: detection is on, the repository is Git and
/// listed.
pub(super) async fn enabled(
    request: &ApiRequest<'_>,
    revision: &Revision,
) -> Result<bool, MethodError> {
    let Some(phid) = &revision.repository_phid else {
        return Ok(false);
    };
    let config = &request.context.config;
    if !config
        .value("merge.conflict.enabled")
        .is_some_and(php::is_truthy)
    {
        return Ok(false);
    }
    let listed = match config.value("merge.conflict.repository-phids") {
        Some(Value::Array(phids)) => phids
            .iter()
            .any(|item| item.as_str() == Some(phid.as_str())),
        _ => false,
    };
    if !listed {
        return Ok(false);
    }
    let found = repository::load(&request.context.database, std::slice::from_ref(phid)).await?;
    Ok(found.iter().any(|repository| repository.vcs == "git"))
}

/// `RevisionMergeConflictStackQuery::loadDescendantPHIDs()`, reading the
/// edit's own writes.
async fn descendants(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    phids: &[String],
) -> Result<Vec<String>, MethodError> {
    let database = &request.context.database;
    let mut seen: Vec<String> = phids.to_vec();
    let mut found: Vec<String> = Vec::new();
    let mut queue: Vec<String> = phids.to_vec();
    while !queue.is_empty() {
        let sql = format!(
            "SELECT dst FROM {} WHERE src IN ({}) AND type = ? ORDER BY src, seq, dateCreated",
            database.table("differential", "edge"),
            phab_db::placeholders(queue.len())
        );
        let query = phab_db::bind_all(phab_db::sqlx::query(&sql), queue.iter().map(String::as_str))
            .bind(edges::REVISION_DEPENDED_ON_BY_REVISION);
        let children = query.fetch_all(&mut *connection).await?;
        queue.clear();
        for row in &children {
            let phid = rows::text(row, "dst")?;
            if seen.contains(&phid) {
                continue;
            }
            seen.push(phid.clone());
            found.push(phid.clone());
            queue.push(phid);
        }
    }
    Ok(found)
}

/// `RevisionMergeConflictWorker::queueCheck()`: a check of `diff`.
async fn queue_check(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    revision: &str,
    diff: &str,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let data = php::json_encode(&json!({"revisionPHID": revision, "diffPHID": diff}));
    let task = worker::Task {
        class: "RevisionMergeConflictWorker",
        data: &data,
        priority: PRIORITY_BULK,
        object_phid: Some(revision),
    };
    worker::schedule_task(connection, database, task, request.now).await?;
    Ok(())
}

/// `queueChecksForPHIDs()`: a check of each open revision with an active
/// diff among `phids`, in the order of `DifferentialRevisionQuery`.
async fn queue_open(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    phids: &[String],
) -> Result<(), MethodError> {
    if phids.is_empty() {
        return Ok(());
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT phid, activeDiffPHID FROM {} WHERE phid IN ({}) AND status NOT IN \
         ('published', 'abandoned') ORDER BY dateModified DESC, id DESC",
        database.table("differential", "differential_revision"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(String::as_str))
        .fetch_all(&mut *connection)
        .await?;
    for row in &found {
        let Some(diff) = rows::opt_text(row, "activeDiffPHID")? else {
            continue;
        };
        let phid = rows::text(row, "phid")?;
        queue_check(request, connection, &phid, &diff).await?;
    }
    Ok(())
}

/// `RevisionMergeConflictWorker::queueChecks()` for each of `revisions`:
/// a check of each open revision with an active diff among them and
/// those stacked above, in the order of `DifferentialRevisionQuery`.
pub(super) async fn queue(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    revisions: &[String],
) -> Result<(), MethodError> {
    for revision in revisions {
        let mut phids = vec![revision.clone()];
        phids.extend(descendants(request, connection, std::slice::from_ref(revision)).await?);
        queue_open(request, connection, &phids).await?;
    }
    Ok(())
}

/// A new diff of `revision`: `queueCheck()` of the diff, then
/// `queueDescendantChecks()` of the revisions stacked above it.
pub(super) async fn queue_diff(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    revision: &str,
    diff: &str,
) -> Result<(), MethodError> {
    queue_check(request, connection, revision, diff).await?;
    let above = descendants(request, connection, &[revision.to_owned()]).await?;
    queue_open(request, connection, &above).await
}

/// Whether an edge type re-wires a stack.
pub(super) fn rewires_stack(edge_type: i64) -> bool {
    matches!(
        edge_type,
        edges::REVISION_DEPENDS_ON_REVISION | edges::REVISION_DEPENDED_ON_BY_REVISION
    )
}
