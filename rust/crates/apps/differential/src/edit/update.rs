//! The `update` transaction (`DifferentialRevisionUpdateTransaction`):
//! attaching a diff to a revision, new or not.
//!
//! What PHP's editor does with it, in order: the review status it moves
//! back to "Needs Review" (`applyInternalEffects()`), the line counts, the
//! repository and the active diff; the diff's `revisionID`; the
//! `differential.revision.void` transaction `expandTransaction()` adds for
//! the reviewers whose rejections (and accepts which aren't sticky) the
//! new diff voids; the affected paths and the local hashes
//! (`applyFinalEffects()`); the diff's view states, copied to the revision
//! (`didCommitTransaction()`); and the merge conflict check of the new
//! diff (`queueMergeConflictChecks()`).
//!
//! Left to PHP: diffs the actor can't see or which belong to a revision
//! already (PHP refuses them, or drops the reattached active diff), diffs
//! with buildables (Harbormaster is told the container changed),
//! Subversion and Mercurial diffs, and diffs of a subdirectory of their
//! repository (`getAffectedPaths()` reads the repository's remote URI).

use std::fmt::Write as _;

use md5::{Digest, Md5};
use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, php};
use phab_db::sqlx::{MySqlConnection, Row};
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use super::{Facts, Plan, PlannedVoid};
use crate::storage::Revision;

/// `ArcanistDifferentialRevisionHash::HASH_GIT_COMMIT`.
const HASH_GIT_COMMIT: &str = "gtcm";
/// `ArcanistDifferentialRevisionHash::HASH_GIT_TREE`.
const HASH_GIT_TREE: &str = "gttr";
/// The width of the `hash` column (`binary(40)`).
const HASH_LENGTH: usize = 40;
/// `array_chunk()` of `lookupOrCreatePaths()`.
const PATH_CHUNK: usize = 128;
/// Rows per insert of affected paths (`chunkSQL()` splits by size).
const ROW_CHUNK: usize = 512;

/// The diff the transaction attaches.
#[derive(Debug, Clone)]
pub(super) struct Diff {
    pub(super) id: i64,
    pub(super) phid: String,
    pub(super) line_count: i64,
    pub(super) repository_phid: Option<String>,
}

/// The planned transaction and its effects.
pub(super) struct Planned {
    /// The `differential:update` transaction.
    pub(super) phid: String,
    /// The active diff before, `null` for a new revision.
    pub(super) old: Option<String>,
    pub(super) diff: Diff,
    /// The void the new diff causes, which follows the update.
    pub(super) void: Option<PlannedVoid>,
    /// The revision's `properties`, encoded, with the new line counts.
    pub(super) properties: String,
    /// `getAffectedPaths()`, in its order.
    pub(super) paths: Vec<String>,
    /// The ID of the revision's new repository.
    pub(super) repository_id: Option<i64>,
    /// `updateRevisionHashTable()`'s hashes: `None` when the diff has no
    /// `local:commits`, which leaves the table alone.
    pub(super) hashes: Option<Vec<(&'static str, String)>>,
}

/// The `update` transaction's value: a diff PHID.
pub(super) fn parse(value: &Value) -> Result<String, MethodError> {
    match value {
        Value::String(phid) if phid.starts_with("PHID-DIFF-") => Ok(phid.clone()),
        _ => Err(MethodError::Unsupported(
            "updates to other than a diff PHID",
        )),
    }
}

/// `normalizePath()`.
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

/// `DiffusionPathIDQuery::expandPathToRoot()`: the path and its parents.
/// A part PHP finds falsy ("0") adds no parent.
fn expand_path_to_root(path: &str) -> Vec<String> {
    let path = normalize_path(path);
    let mut parents = vec![path.clone()];
    let mut parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    while let Some(last) = parts.pop() {
        if !last.is_empty() && last != "0" {
            parents.push(format!("/{}", parts.join("/")));
        }
    }
    parents
}

/// `getAffectedPaths()` of these file names, without a path prefix.
fn affected_paths(filenames: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for filename in filenames {
        for path in expand_path_to_root(&format!("/{filename}")) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// The diff's columns the transaction reads.
struct DiffRow {
    diff: Diff,
    revision_id: Option<i64>,
    vcs: Option<String>,
    source_control_path: Option<String>,
}

async fn load_diff(database: &Database, phid: &str) -> Result<Option<DiffRow>, MethodError> {
    let sql = format!(
        "SELECT id, phid, revisionID, repositoryPHID, sourceControlSystem, sourceControlPath, \
         lineCount FROM {} WHERE phid = ?",
        database.table("differential", "differential_diff")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(DiffRow {
        diff: Diff {
            id: rows::int(&row, "id")?,
            phid: rows::text(&row, "phid")?,
            line_count: rows::int(&row, "lineCount")?,
            repository_phid: rows::opt_text(&row, "repositoryPHID")?,
        },
        revision_id: rows::opt_int(&row, "revisionID")?,
        vcs: rows::opt_text(&row, "sourceControlSystem")?,
        source_control_path: rows::opt_text(&row, "sourceControlPath")?,
    }))
}

/// The changesets' file names, newest first as `DifferentialChangesetQuery`
/// reads them, and the lines they add and remove.
async fn changesets(
    database: &Database,
    diff_id: i64,
) -> Result<(Vec<String>, i64, i64), MethodError> {
    let sql = format!(
        "SELECT filename, addLines, delLines FROM {} WHERE diffID = ? ORDER BY id DESC",
        database.table("differential", "differential_changeset")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .fetch_all(database.pool())
        .await?;
    let (mut filenames, mut added, mut removed) = (Vec::new(), 0, 0);
    for row in &found {
        let bytes: Vec<u8> = row
            .try_get("filename")
            .map_err(|_| MethodError::internal("a changeset without a file name"))?;
        let filename = String::from_utf8(bytes)
            .map_err(|_| MethodError::Unsupported("file names which are not UTF-8"))?;
        filenames.push(filename);
        added += rows::int(row, "addLines")?;
        removed += rows::int(row, "delLines")?;
    }
    Ok((filenames, added, removed))
}

/// `$diff->getSourceControlPath()` as `getAffectedPaths()` reads it: a
/// root (or none) adds no prefix; other paths are left to PHP.
fn check_source_path(path: Option<&str>, has_repository: bool) -> Result<(), MethodError> {
    let path = path.unwrap_or_default();
    let falsy = path.is_empty() || path == "0";
    if falsy || !has_repository || path == "/" {
        return Ok(());
    }
    Err(MethodError::Unsupported("diffs of a subdirectory"))
}

/// The hashes of the diff's `local:commits`, `None` without the property.
/// Only Git diffs have hashes.
async fn local_hashes(
    database: &Database,
    diff_id: i64,
    is_git: bool,
) -> Result<Option<Vec<(&'static str, String)>>, MethodError> {
    let sql = format!(
        "SELECT data FROM {} WHERE diffID = ? AND name = 'local:commits'",
        database.table("differential", "differential_diffproperty")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .fetch_optional(database.pool())
        .await?
    else {
        return Ok(None);
    };
    let commits: Vec<Value> = match rows::json(&row, "data")? {
        Value::Object(map) => map.into_iter().map(|(_, commit)| commit).collect(),
        Value::Array(list) => list,
        _ => return Err(MethodError::Unsupported("unusual local commits")),
    };
    let mut hashes = Vec::new();
    if !is_git {
        return Ok(Some(hashes));
    }
    for commit in &commits {
        for (kind, key) in [(HASH_GIT_COMMIT, "commit"), (HASH_GIT_TREE, "tree")] {
            match commit.get(key) {
                Some(Value::String(hash)) if hash.len() <= HASH_LENGTH => {
                    hashes.push((kind, hash.clone()));
                }
                _ => return Err(MethodError::Unsupported("unusual local commits")),
            }
        }
    }
    Ok(Some(hashes))
}

/// The revision's `properties` with the new line counts
/// (`setAddedLineCount()`, `setRemovedLineCount()`): existing keys keep
/// their place.
async fn properties(
    database: &Database,
    revision: &Revision,
    is_new: bool,
    (added, removed): (i64, i64),
) -> Result<String, MethodError> {
    let mut map = if is_new {
        // initializeNewRevision(): a draft which doesn't broadcast.
        let mut map = Map::new();
        map.insert("draft.broadcast".to_owned(), json!(false));
        map
    } else {
        let sql = format!(
            "SELECT properties FROM {} WHERE id = ?",
            database.table("differential", "differential_revision")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(revision.id)
            .fetch_one(database.pool())
            .await?;
        match php::round_trip(&rows::json(&row, "properties")?) {
            Value::Object(map) => map,
            Value::Array(list) if list.is_empty() => Map::new(),
            _ => return Err(MethodError::Unsupported("unusual revision properties")),
        }
    };
    map.insert("lines.added".to_owned(), json!(added));
    map.insert("lines.removed".to_owned(), json!(removed));
    crate::diff_editor::encode(&Value::Object(map))
}

/// Whether the diff has a buildable, whose container Harbormaster is told
/// changed (`didCommitTransaction()`).
async fn has_buildable(database: &Database, diff: &str) -> Result<bool, MethodError> {
    let sql = format!(
        "SELECT 1 FROM {} WHERE buildablePHID = ? LIMIT 1",
        database.table("harbormaster", "harbormaster_buildable")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(diff)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// Plans the transaction once PHP's validation and capability checks
/// pass, and applies its internal effects to `revision`: the status, the
/// repository and the active diff. The void is planned by the caller,
/// which knows the reviewers.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    diff_phid: &str,
    revision: &mut Revision,
    facts: &Facts,
    is_new: bool,
) -> Result<Planned, MethodError> {
    let context = request.context;
    let database = &context.database;
    if !is_new && !super::can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    let visible = crate::diff_search::visible_diffs(request, &[Phid::new(diff_phid)]).await?;
    let found = match load_diff(database, diff_phid).await? {
        Some(found) if !visible.is_empty() => found,
        // "Specified diff does not exist."
        _ => return Err(MethodError::Unsupported("diffs PHP refuses")),
    };
    // Attached to this revision or another: refused, or a reattached
    // active diff, which PHP drops.
    if found.revision_id.is_some() {
        return Err(MethodError::Unsupported("diffs attached to revisions"));
    }
    match found.vcs.as_deref() {
        Some("svn") => return Err(MethodError::Unsupported("Subversion diffs")),
        Some("hg") => return Err(MethodError::Unsupported("Mercurial diffs")),
        _ => {}
    }
    let diff = found.diff.clone();
    if diff.repository_phid.as_deref() == Some("") {
        return Err(MethodError::Unsupported("diffs with an empty repository"));
    }
    check_source_path(
        found.source_control_path.as_deref(),
        diff.repository_phid.is_some(),
    )?;
    if has_buildable(database, &diff.phid).await? {
        return Err(MethodError::Unsupported("diffs with buildables"));
    }
    let (filenames, added, removed) = changesets(database, diff.id).await?;
    let properties = properties(database, revision, is_new, (added, removed)).await?;
    let is_git = found.vcs.as_deref() == Some("git");
    let hashes = local_hashes(database, diff.id, is_git).await?;
    let repository_id = match &diff.repository_phid {
        Some(phid) => {
            let found = crate::repository::load(database, &[Phid::new(phid.as_str())]).await?;
            match found.first() {
                Some(repository) => Some(repository.id),
                None => return Err(MethodError::Unsupported("diffs of unknown repositories")),
            }
        }
        None => None,
    };

    // applyInternalEffects(): updating a revision sent back to its author
    // asks for review again; drafts stay drafts.
    if matches!(
        revision.status.as_str(),
        "needs-revision" | "changes-planned" | "abandoned"
    ) {
        let status = if facts.should_broadcast {
            "needs-review"
        } else {
            "draft"
        };
        status.clone_into(&mut revision.status);
    }
    let old = revision
        .active_diff_phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned());
    revision.repository_phid = diff.repository_phid.as_deref().map(Phid::new);
    revision.active_diff_phid = Some(Phid::new(diff.phid.as_str()));

    // applyFinalEffects() reloads the revision, which fails if the actor
    // can't see it with its new repository.
    let visible = crate::storage::visible_revisions(
        database,
        &context.config,
        &request.viewer,
        vec![revision.clone()],
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if visible.is_empty() {
        return Err(MethodError::Unsupported(
            "revisions the actor can't see once updated",
        ));
    }
    Ok(Planned {
        phid: super::transaction_phid(),
        old,
        diff,
        void: None,
        properties,
        paths: affected_paths(&filenames),
        repository_id,
        hashes,
    })
}

/// The revision columns the transaction sets: the line count, the
/// properties, the repository and the active diff.
pub(super) async fn write_revision(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut MySqlConnection,
) -> Result<(), MethodError> {
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET lineCount = ?, properties = ?, repositoryPHID = ?, activeDiffPHID = ? \
         WHERE id = ?",
        request
            .context
            .database
            .table("differential", "differential_revision")
    ))
    .bind(planned.diff.line_count)
    .bind(&planned.properties)
    .bind(&planned.diff.repository_phid)
    .bind(&planned.diff.phid)
    .bind(plan.revision.id)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The transaction, the diff's `revisionID` (`applyExternalEffects()`),
/// then the void it causes.
pub(super) async fn write_transaction(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    metadata: &str,
    connection: &mut MySqlConnection,
) -> Result<(), MethodError> {
    let (old, new) = (
        php::json_encode(&json!(planned.old)),
        php::json_encode(&json!(planned.diff.phid)),
    );
    let row = (
        planned.phid.as_str(),
        "differential:update",
        &*old,
        &*new,
        metadata,
    );
    super::insert_transaction(request, plan, connection, row).await?;
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET revisionID = ?, dateModified = ? WHERE id = ?",
        request
            .context
            .database
            .table("differential", "differential_diff")
    ))
    .bind(plan.revision.id)
    .bind(request.now)
    .bind(planned.diff.id)
    .execute(&mut *connection)
    .await?;
    if let Some((phid, reviewers, force)) = &planned.void {
        super::write_void(request, plan, (phid, reviewers, force), connection).await?;
    }
    Ok(())
}

/// `lookupPaths()`: the IDs of the known paths, as MySQL returns them.
async fn lookup_paths(
    database: &Database,
    connection: &mut MySqlConnection,
    paths: &[String],
) -> Result<Vec<(String, i64)>, MethodError> {
    let mut found = Vec::new();
    for chunk in paths.chunks(PATH_CHUNK) {
        let sql = format!(
            "SELECT path, id FROM {} WHERE pathHash IN ({})",
            database.table("repository", "repository_path"),
            phab_db::placeholders(chunk.len())
        );
        let hashes: Vec<String> = chunk.iter().map(|path| path_hash(path)).collect();
        let rows = phab_db::bind_all(
            phab_db::sqlx::query(&sql),
            hashes.iter().map(String::as_str),
        )
        .fetch_all(&mut *connection)
        .await?;
        for row in &rows {
            let (path, id) = (rows::text(row, "path")?, rows::int(row, "id")?);
            match found.iter_mut().find(|(known, _)| *known == path) {
                Some(entry) => entry.1 = id,
                None => found.push((path, id)),
            }
        }
    }
    Ok(found)
}

/// `md5()` of a path, in hexadecimal.
fn path_hash(path: &str) -> String {
    Md5::digest(path.as_bytes())
        .iter()
        .fold(String::with_capacity(32), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// `lookupOrCreatePaths()`: the paths' IDs, the known ones first, as
/// MySQL returns them, then the new ones.
async fn path_ids(
    database: &Database,
    connection: &mut MySqlConnection,
    paths: &[String],
) -> Result<Vec<i64>, MethodError> {
    let mut found = lookup_paths(database, connection, paths).await?;
    let missing: Vec<String> = paths
        .iter()
        .filter(|path| !found.iter().any(|(known, _)| known == *path))
        .cloned()
        .collect();
    if !missing.is_empty() {
        for chunk in missing.chunks(PATH_CHUNK) {
            let sql = format!(
                "INSERT IGNORE INTO {} (path, pathHash) VALUES {}",
                database.table("repository", "repository_path"),
                vec!["(?, ?)"; chunk.len()].join(", ")
            );
            let mut query = phab_db::sqlx::query(&sql);
            for path in chunk {
                query = query.bind(path.as_str()).bind(path_hash(path));
            }
            query.execute(&mut *connection).await?;
        }
        for (path, id) in lookup_paths(database, connection, &missing).await? {
            if !found.iter().any(|(known, _)| *known == path) {
                found.push((path, id));
            }
        }
    }
    Ok(found.into_iter().map(|(_, id)| id).collect())
}

/// `applyFinalEffects()`: the affected paths (`updateAffectedPaths()`) and
/// the local hashes (`updateRevisionHashTable()`).
pub(super) async fn write_final(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.id;
    let ids = path_ids(database, connection, &planned.paths).await?;
    let table = database.table("differential", "differential_affectedpath");
    phab_db::sqlx::query(&format!("DELETE FROM {table} WHERE revisionID = ?"))
        .bind(revision)
        .execute(&mut *connection)
        .await?;
    for chunk in ids.chunks(ROW_CHUNK) {
        let sql = format!(
            "INSERT INTO {table} (repositoryID, pathID, revisionID) VALUES {}",
            vec!["(?, ?, ?)"; chunk.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for id in chunk {
            query = query.bind(planned.repository_id).bind(id).bind(revision);
        }
        query.execute(&mut *connection).await?;
    }
    let Some(hashes) = &planned.hashes else {
        return Ok(());
    };
    let table = database.table("differential", "differential_revisionhash");
    phab_db::sqlx::query(&format!("DELETE FROM {table} WHERE revisionID = ?"))
        .bind(revision)
        .execute(&mut *connection)
        .await?;
    if !hashes.is_empty() {
        let sql = format!(
            "INSERT INTO {table} (revisionID, type, hash) VALUES {}",
            vec!["(?, ?, ?)"; hashes.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for (kind, hash) in hashes {
            query = query.bind(revision).bind(*kind).bind(hash.as_str());
        }
        query.execute(&mut *connection).await?;
    }
    Ok(())
}

/// `DifferentialViewState::copyViewStatesToObject()`: the view states of
/// the diff become the revision's, but for viewers who have one already.
pub(super) async fn write_view_states(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut MySqlConnection,
) -> Result<(), MethodError> {
    let table = request
        .context
        .database
        .table("differential", "differential_viewstate");
    phab_db::sqlx::query(&format!(
        "INSERT IGNORE INTO {table} (viewerPHID, objectPHID, viewState, dateCreated, \
         dateModified) SELECT viewerPHID, ?, viewState, dateCreated, dateModified FROM {table} \
         WHERE objectPHID = ?"
    ))
    .bind(plan.revision.phid.as_str())
    .bind(&planned.diff.phid)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_paths_like_php() {
        assert_eq!(
            expand_path_to_root("/docs//guide/intro.md"),
            ["/docs/guide/intro.md", "/docs/guide", "/docs", "/"]
        );
        assert_eq!(expand_path_to_root("/"), ["/"]);
        // A part named "0" is falsy: its parent isn't listed.
        assert_eq!(expand_path_to_root("/a/0"), ["/a/0", "/"]);
        assert_eq!(
            affected_paths(&["README.md".to_owned(), "docs/a.md".to_owned()]),
            ["/README.md", "/", "/docs/a.md", "/docs"]
        );
    }

    #[test]
    fn hashes_paths_like_php() {
        assert_eq!(path_hash("/"), "6666cd76f96956469e7be39d750cc7d9");
    }

    #[test]
    fn reads_source_paths_like_php() {
        assert!(check_source_path(None, true).is_ok());
        assert!(check_source_path(Some("/"), true).is_ok());
        assert!(check_source_path(Some("/trunk"), false).is_ok());
        assert!(check_source_path(Some("/trunk"), true).is_err());
    }

    #[test]
    fn parses_diff_phids() {
        assert_eq!(parse(&json!("PHID-DIFF-abc")).unwrap(), "PHID-DIFF-abc");
        assert!(parse(&json!(["PHID-DIFF-abc"])).is_err());
        assert!(parse(&json!("PHID-DREV-abc")).is_err());
    }
}
