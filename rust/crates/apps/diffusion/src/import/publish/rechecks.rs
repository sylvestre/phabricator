//! `queueMergeConflictRechecks()`: once a commit lands on the default
//! branch of a repository Mozilla's merge conflict detection checks
//! (`RevisionMergeConflictWorker::isEnabledForRepository()`), a check of
//! each open revision of the repository touching a file the commit
//! changed, and of the revisions stacked above them
//! (`RevisionMergeConflictWorker::queueChecks()`), at bulk priority.
//!
//! Left to PHP: a `git merge-base` which times out, which PHP takes as "not
//! on the branch".

use std::time::Duration;

use md5::{Digest as _, Md5};
use phab_conduit::Context;
use phab_core::php;
use phab_db::{Database, edges, rows, worker};
use serde_json::{Value, json};

use crate::git::ref_selector;
use crate::import::ImportError;
use crate::import::commit::ParserTask;
use crate::import::publish::audit::{FILE_DIRECTORY, path_changes};

/// `RevisionMergeConflictEngine::GIT_TIMEOUT_SECONDS`.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);
/// `PhabricatorWorker::PRIORITY_BULK`.
const PRIORITY_BULK: i64 = 3000;
/// The task class.
const MERGE_CONFLICT_WORKER: &str = "RevisionMergeConflictWorker";

/// A queued check (`RevisionMergeConflictWorker::queueCheck()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    revision_phid: String,
    diff_phid: String,
    trigger_commit: String,
}

/// `isEnabledForRepository()`: detection is on, and the repository is a
/// listed Git one.
fn is_enabled(context: &Context, task: &ParserTask) -> bool {
    let config = &context.config;
    if !config
        .value("merge.conflict.enabled")
        .is_some_and(php::is_truthy)
    {
        return false;
    }
    if task.repository.vcs != "git" {
        return false;
    }
    match config.value("merge.conflict.repository-phids") {
        Some(Value::Array(phids)) => phids
            .iter()
            .any(|phid| phid.as_str() == Some(task.repository.phid.as_str())),
        _ => false,
    }
}

/// `isCommitOnDefaultBranch()`: `git merge-base --is-ancestor` exits 0.
async fn is_on_default_branch(task: &ParserTask) -> Result<bool, ImportError> {
    let branch = task.repository.default_branch();
    let copy = task.repository.working_copy()?;
    let output = copy
        .exec_with_timeout(
            vec![
                "merge-base".to_owned(),
                "--is-ancestor".to_owned(),
                ref_selector(&task.commit.identifier)?,
                format!("refs/heads/{branch}"),
            ],
            None,
            GIT_TIMEOUT,
        )
        .await?;
    Ok(output.success())
}

/// `DiffusionPathIDQuery::normalizePath()`.
fn normalize_path(path: &str) -> String {
    let mut collapsed = String::with_capacity(path.len());
    for character in path.chars() {
        if character == '/' && collapsed.ends_with('/') {
            continue;
        }
        collapsed.push(character);
    }
    format!("/{}", collapsed.trim_matches('/'))
}

/// `DiffusionPathIDQuery::loadPathIDs()`: the IDs of the paths which have
/// one.
async fn path_ids(database: &Database, paths: &[String]) -> Result<Vec<i64>, ImportError> {
    let mut hashes: Vec<String> = Vec::new();
    for path in paths {
        let hash = hex::encode(Md5::digest(normalize_path(path).as_bytes()));
        if !hashes.contains(&hash) {
            hashes.push(hash);
        }
    }
    let sql = format!(
        "SELECT id FROM {} WHERE pathHash IN ({})",
        database.table("repository", "repository_path"),
        phab_db::placeholders(hashes.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        hashes.iter().map(String::as_str),
    )
    .fetch_all(database.pool())
    .await?;
    found
        .iter()
        .map(|row| rows::int(row, "id").map_err(ImportError::Database))
        .collect()
}

/// The open revisions of the repository affecting these paths
/// (`DifferentialRevisionQuery::withPaths()`).
async fn revisions_with_paths(
    database: &Database,
    task: &ParserTask,
    ids: &[i64],
) -> Result<Vec<String>, ImportError> {
    let sql = format!(
        "SELECT r.phid FROM {} r JOIN {} paths ON paths.revisionID = r.id WHERE \
         paths.pathID IN ({}) AND paths.repositoryID = ? AND r.repositoryPHID = ? AND r.status \
         NOT IN ('published', 'abandoned') GROUP BY r.id",
        database.table("differential", "differential_revision"),
        database.table("differential", "differential_affectedpath"),
        phab_db::placeholders(ids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
        .bind(task.repository.id)
        .bind(&task.repository.phid)
        .fetch_all(database.pool())
        .await?;
    found
        .iter()
        .map(|row| rows::text(row, "phid").map_err(ImportError::Database))
        .collect()
}

/// `RevisionMergeConflictStackQuery::loadDescendantPHIDs()`.
async fn descendants(database: &Database, phids: &[String]) -> Result<Vec<String>, ImportError> {
    let mut seen: Vec<String> = phids.to_vec();
    let mut found: Vec<String> = Vec::new();
    let mut queue: Vec<String> = phids.to_vec();
    while !queue.is_empty() {
        let sql = format!(
            "SELECT dst FROM {} WHERE src IN ({}) AND type = ? ORDER BY src, seq, dateCreated",
            database.table("differential", "edge"),
            phab_db::placeholders(queue.len())
        );
        let children =
            phab_db::bind_all(phab_db::sqlx::query(&sql), queue.iter().map(String::as_str))
                .bind(edges::REVISION_DEPENDED_ON_BY_REVISION)
                .fetch_all(database.pool())
                .await?;
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

/// `queueChecksForPHIDs()`: the open revisions among `phids` with an
/// active diff (their newest), in the order of `DifferentialRevisionQuery`.
async fn open_checks(
    database: &Database,
    phids: &[String],
    trigger_commit: &str,
) -> Result<Vec<Check>, ImportError> {
    let sql = format!(
        "SELECT r.phid, (SELECT d.phid FROM {} d WHERE d.revisionID = r.id ORDER BY d.id DESC \
         LIMIT 1) diffPHID FROM {} r WHERE r.phid IN ({}) AND r.status NOT IN ('published', \
         'abandoned') ORDER BY r.dateModified DESC, r.id DESC",
        database.table("differential", "differential_diff"),
        database.table("differential", "differential_revision"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?;
    let mut checks = Vec::new();
    for row in &found {
        let Some(diff_phid) = rows::opt_text(row, "diffPHID")? else {
            continue;
        };
        checks.push(Check {
            revision_phid: rows::text(row, "phid")?,
            diff_phid,
            trigger_commit: trigger_commit.to_owned(),
        });
    }
    Ok(checks)
}

/// The checks the commit queues.
pub async fn plan(context: &Context, task: &ParserTask) -> Result<Vec<Check>, ImportError> {
    let database = &context.database;
    if !is_enabled(context, task) || !is_on_default_branch(task).await? {
        return Ok(Vec::new());
    }
    // getChangedFilePaths(): files only.
    let paths: Vec<String> = path_changes(database, task.commit.id)
        .await?
        .into_iter()
        .filter(|change| change.file_type != FILE_DIRECTORY)
        .map(|change| change.path)
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let ids = path_ids(database, &paths).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut phids = revisions_with_paths(database, task, &ids).await?;
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    phids.extend(descendants(database, &phids).await?);
    open_checks(database, &phids, &task.commit.identifier).await
}

/// Queues the checks.
pub async fn queue(database: &Database, checks: &[Check], now: i64) -> Result<(), ImportError> {
    if checks.is_empty() {
        return Ok(());
    }
    let mut connection = database.pool().acquire().await?;
    for check in checks {
        let data = php::json_encode(&json!({
            "revisionPHID": check.revision_phid,
            "diffPHID": check.diff_phid,
            "triggerCommit": check.trigger_commit,
        }));
        worker::schedule_task(
            &mut connection,
            database,
            worker::Task {
                class: MERGE_CONFLICT_WORKER,
                data: &data,
                priority: PRIORITY_BULK,
                object_phid: Some(&check.revision_phid),
            },
            now,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_like_php() {
        assert_eq!(normalize_path("dir//a.txt"), "/dir/a.txt");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("a/"), "/a");
    }
}
