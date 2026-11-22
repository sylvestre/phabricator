//! `DiffusionQuery::loadHistoryForCommitIdentifiers()`: the commits of a
//! list of hashes as `DiffusionPathChange` dictionaries, and the path
//! utilities it uses (`DiffusionPathIDQuery`).

use std::collections::HashMap;

use phab_conduit::MethodError;
use phab_core::php as core_php;
use phab_db::{Database, rows};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;

/// `DiffusionPathIDQuery::normalizePath()`: single slashes, one leading
/// slash, no trailing one.
pub fn normalize_path(path: &str) -> String {
    let mut single = String::with_capacity(path.len());
    for character in path.chars() {
        if character == '/' && single.ends_with('/') {
            continue;
        }
        single.push(character);
    }
    format!("/{}", single.trim_matches('/'))
}

/// The ID of a normalized path in `repository_path`, if it has one.
pub async fn path_id(database: &Database, normal: &str) -> Result<Option<i64>, MethodError> {
    let sql = format!(
        "SELECT id, path FROM {} WHERE pathHash = MD5(?)",
        database.table("repository", "repository_path")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(normal)
        .fetch_all(database.pool())
        .await?;
    // ipull($paths, 'id', 'path'): the last row of the path wins.
    let mut id = None;
    for row in &found {
        if rows::text(row, "path")? == normal {
            id = Some(rows::int(row, "id")?);
        }
    }
    Ok(id)
}

/// A `repository_commit` row, as `toDictionary()` shows it (Lisk keeps
/// MySQL's strings).
struct Commit {
    id: i64,
    dictionary: Value,
}

async fn load_commits(
    database: &Database,
    repository_id: i64,
    identifiers: &[String],
) -> Result<HashMap<String, Commit>, MethodError> {
    let sql = format!(
        "SELECT id, repositoryID, phid, commitIdentifier, epoch, authorPHID, auditStatus, \
         summary, importStatus FROM {} WHERE repositoryID = ? AND commitIdentifier IN ({})",
        database.table("repository", "repository_commit"),
        phab_db::placeholders(identifiers.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql).bind(repository_id),
        identifiers.iter(),
    )
    .fetch_all(database.pool())
    .await?;
    let mut commits = HashMap::new();
    for row in &found {
        let identifier = rows::text(row, "commitIdentifier")?;
        commits.insert(
            identifier.clone(),
            Commit {
                id: rows::int(row, "id")?,
                dictionary: json!({
                    "repositoryID": rows::int(row, "repositoryID")?.to_string(),
                    "phid": rows::text(row, "phid")?,
                    "commitIdentifier": identifier,
                    "epoch": rows::int(row, "epoch")?.to_string(),
                    "authorPHID": rows::opt_text(row, "authorPHID")?,
                    "auditStatus": rows::text(row, "auditStatus")?,
                    "summary": rows::text(row, "summary")?,
                    "importStatus": rows::int(row, "importStatus")?.to_string(),
                }),
            },
        );
    }
    Ok(commits)
}

/// `PhabricatorRepositoryCommitData::toDictionary()` of the commits with
/// data, by commit ID.
async fn load_data(database: &Database, ids: &[i64]) -> Result<HashMap<i64, Value>, MethodError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT commitID, authorName, commitMessage, commitDetails FROM {} WHERE commitID IN ({})",
        database.table("repository", "repository_commitdata"),
        phab_db::placeholders(ids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids)
        .fetch_all(database.pool())
        .await?;
    let mut data = HashMap::new();
    for row in &found {
        let details @ (Value::Object(_) | Value::Array(_)) = rows::json(row, "commitDetails")?
        else {
            return Err(MethodError::Unsupported("unusual commit details"));
        };
        let commit_id = rows::int(row, "commitID")?;
        data.insert(
            commit_id,
            json!({
                "commitID": commit_id.to_string(),
                "authorName": rows::opt_text(row, "authorName")?,
                "commitMessage": rows::opt_text(row, "commitMessage")?,
                "commitDetails": core_php::json_encode(&core_php::round_trip(&details)),
            }),
        );
    }
    Ok(data)
}

/// The `fileType` and `changeType` of the path changes of `path_id` in
/// these commits, by commit ID.
async fn load_changes(
    database: &Database,
    commit_ids: &[i64],
    path_id: i64,
) -> Result<HashMap<i64, (String, String)>, MethodError> {
    let sql = format!(
        "SELECT commitID, changeType, fileType FROM {} WHERE commitID IN ({}) AND pathID = ?",
        database.table("repository", "repository_pathchange"),
        phab_db::placeholders(commit_ids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), commit_ids)
        .bind(path_id)
        .fetch_all(database.pool())
        .await?;
    let mut changes = HashMap::new();
    for row in &found {
        changes.insert(
            rows::int(row, "commitID")?,
            (
                rows::int(row, "changeType")?.to_string(),
                rows::int(row, "fileType")?.to_string(),
            ),
        );
    }
    Ok(changes)
}

/// `loadHistoryForCommitIdentifiers()`, then `toDictionary()` of each
/// path change: commits not imported yet are ephemeral commit objects.
pub async fn load_history(
    database: &Database,
    drequest: &DiffusionRequest,
    identifiers: &[String],
) -> Result<Vec<Value>, MethodError> {
    if identifiers.is_empty() {
        return Ok(Vec::new());
    }
    let repository_id = drequest.repository.id;
    let commits = load_commits(database, repository_id, identifiers).await?;
    let mut commit_ids: Vec<i64> = commits.values().map(|commit| commit.id).collect();
    commit_ids.sort_unstable();
    let data = load_data(database, &commit_ids).await?;
    let normal = normalize_path(&drequest.path()?);
    let path_id = path_id(database, &normal).await?;
    let changes = match path_id {
        Some(path_id) if !commit_ids.is_empty() => {
            load_changes(database, &commit_ids, path_id).await?
        }
        _ => HashMap::new(),
    };
    let mut history = Vec::new();
    for identifier in identifiers {
        let (commit, commit_data, change) = match commits.get(identifier) {
            Some(commit) => (
                commit.dictionary.clone(),
                data.get(&commit.id).cloned().unwrap_or_else(|| json!([])),
                changes.get(&commit.id),
            ),
            None => (
                json!({
                    "repositoryID": repository_id.to_string(),
                    "phid": null,
                    "commitIdentifier": identifier,
                    "epoch": null,
                    "authorPHID": null,
                    "auditStatus": "none",
                    "summary": "",
                    "importStatus": 0,
                }),
                json!([]),
                None,
            ),
        };
        history.push(json!({
            "path": null,
            "commitIdentifier": identifier,
            "commit": commit,
            "commitData": commit_data,
            "fileType": change.map(|(_, file_type)| file_type),
            "changeType": change.map(|(change_type, _)| change_type),
            "targetPath": null,
            "targetCommitIdentifier": null,
            "awayPaths": [],
        }));
    }
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("a//b/"), "/a/b");
        assert_eq!(normalize_path("///"), "/");
        assert_eq!(normalize_path("/x"), "/x");
    }
}
