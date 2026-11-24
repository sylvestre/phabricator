//! `diffusion.updatecoverage` (`DiffusionUpdateCoverageConduitAPIMethod`):
//! stores the coverage of paths at a commit of a branch in
//! `repository_coverage`, creating the branch if it has no row yet; the
//! `overwrite` mode first deletes the branch's coverage.
//!
//! Ported: every parameter. Left to PHP: repositories the viewer can't see
//! (`executeOne()` raises a policy exception), commits named with a
//! repository's monogram, Subversion repositories, paths which are not in
//! `repository_path` (PHP formats their missing IDs), an empty coverage
//! map, branch names longer than their column, values of unusual types and
//! shadow calls.

use phab_app_differential::repository;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, edit, pager};
use phab_core::Phid;
use phab_db::rows;
use serde_json::Value;

use crate::history::normalize_path;
use crate::last_modified_query::split_monogram;

/// `PhabricatorRepository::MINIMUM_QUALIFIED_HASH`.
const MINIMUM_QUALIFIED_HASH: usize = 5;
/// The length of the `name` column of `repository_branch` (`text128`).
const BRANCH_LENGTH: usize = 128;
/// `PhabricatorLiskDAO::chunkSQL()`'s limit: 1MB minus 10%.
const CHUNK_LIMIT: usize = 943_718;

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// A string parameter.
fn string(request: &ApiRequest<'_>, key: &str) -> Result<String, MethodError> {
    match request.value(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        _ => Err(unsupported("parameters which are not strings")),
    }
}

/// How the coverage is written (`mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `overwrite`, the default: the branch's coverage is replaced.
    Overwrite,
    /// `update`: only the given paths change.
    Update,
}

/// `mode`, or the message of PHP's exception for an invalid one.
fn mode(request: &ApiRequest<'_>) -> Result<Result<Mode, String>, MethodError> {
    match request.value("mode") {
        None | Some(Value::Null) => Ok(Ok(Mode::Overwrite)),
        Some(Value::String(mode)) => Ok(match mode.as_str() {
            "" | "overwrite" => Ok(Mode::Overwrite),
            "update" => Ok(Mode::Update),
            _ => Err(format!("Invalid mode \"{mode}\".")),
        }),
        Some(_) => Err(unsupported("modes which are not strings")),
    }
}

/// `DiffusionCommitQuery::withRepository()->withIdentifiers()` of one
/// name, then `executeOne()`: the commit's ID, if one matches.
async fn commit_id(
    request: &ApiRequest<'_>,
    repository_id: i64,
    name: &str,
) -> Result<Option<i64>, MethodError> {
    if name.contains('\n') {
        return Err(unsupported("commit names with newlines"));
    }
    let (scope, hash) = split_monogram(name);
    if scope.is_some() {
        return Err(unsupported("commits named with a repository's monogram"));
    }
    // Too short (or "0", which `nonempty()` drops): no identifier, so no
    // commit.
    if hash.len() < MINIMUM_QUALIFIED_HASH {
        return Ok(None);
    }
    let (condition, value) = if hash.len() == 40 {
        ("commitIdentifier = ?", hash.to_owned())
    } else {
        (
            "commitIdentifier LIKE ?",
            format!("{}%", phab_db::like_escape(hash)),
        )
    };
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE repositoryID = ? AND {condition} LIMIT 2",
        database.table("repository", "repository_commit"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(repository_id)
        .bind(value)
        .fetch_all(database.pool())
        .await?;
    match found.as_slice() {
        [] => Ok(None),
        [row] => Ok(Some(rows::int(row, "id")?)),
        _ => Err(pager::core_error("Expected a single result!".to_owned())),
    }
}

/// `PhabricatorRepositoryBranch::loadOrCreateBranch()`: the ID of the
/// branch's row, inserted if there is none.
async fn branch_id(
    request: &ApiRequest<'_>,
    repository_id: i64,
    name: &str,
) -> Result<i64, MethodError> {
    let database = &request.context.database;
    let table = database.table("repository", "repository_branch");
    let sql = format!("SELECT id FROM {table} WHERE repositoryID = ? AND name = ?");
    let found = phab_db::sqlx::query(&sql)
        .bind(repository_id)
        .bind(name)
        .fetch_optional(database.pool())
        .await?;
    if let Some(row) = found {
        return Ok(rows::int(&row, "id")?);
    }
    let sql = format!(
        "INSERT INTO {table} (repositoryID, name, lintCommit, dateCreated, dateModified) \
         VALUES (?, ?, NULL, ?, ?)"
    );
    let result = phab_db::sqlx::query(&sql)
        .bind(repository_id)
        .bind(name)
        .bind(request.now)
        .bind(request.now)
        .execute(database.pool())
        .await?;
    i64::try_from(result.last_insert_id()).map_err(MethodError::internal)
}

/// `DiffusionPathIDQuery::loadPathIDs()`: the ID of each path, which every
/// path must have.
async fn path_ids(request: &ApiRequest<'_>, paths: &[&str]) -> Result<Vec<i64>, MethodError> {
    let normals: Vec<String> = paths.iter().map(|path| normalize_path(path)).collect();
    let mut hashes: Vec<String> = normals
        .iter()
        .map(|normal| crate::import::change::md5_hex(normal))
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    let database = &request.context.database;
    let sql = format!(
        "SELECT id, path FROM {} WHERE pathHash IN ({})",
        database.table("repository", "repository_path"),
        phab_db::placeholders(hashes.len()),
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), hashes.iter())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| Ok((rows::text(row, "path")?, rows::int(row, "id")?)))
        .collect::<Result<std::collections::HashMap<String, i64>, phab_db::DbError>>()?;
    normals
        .iter()
        .map(|normal| {
            found
                .get(normal)
                .copied()
                .ok_or_else(|| unsupported("paths which are not in repository_path"))
        })
        .collect()
}

/// The length of the fragment `qsprintf()` builds for `coverage` (`(%d,
/// %d, %d, %s)`), the string escaped as `mysqli::real_escape_string()`
/// does.
fn fragment_length(ids: [i64; 3], coverage: &str) -> usize {
    let escaped: usize = coverage
        .bytes()
        .map(|byte| match byte {
            0 | b'\n' | b'\r' | b'\\' | b'\'' | b'"' | 0x1a => 2,
            _ => 1,
        })
        .sum();
    let digits: usize = ids.iter().map(|id| id.to_string().len()).sum();
    // "(", three ", ", two quotes and ")".
    digits + escaped + 10
}

/// `PhabricatorLiskDAO::chunkSQL()`: the number of fragments in each
/// chunk.
fn chunk_sizes(lengths: &[usize]) -> Vec<usize> {
    const GLUE: usize = 2;
    let mut sizes = Vec::new();
    let (mut count, mut length) = (0, 0);
    for this in lengths {
        let this = if count > 0 { this + GLUE } else { *this };
        if length + this <= CHUNK_LIMIT {
            length += this;
            count += 1;
        } else {
            if count > 0 {
                sizes.push(count);
            }
            length = this - GLUE;
            count = 1;
        }
    }
    if count > 0 {
        sizes.push(count);
    }
    sizes
}

/// `diffusion.updatecoverage`.
pub struct UpdateCoverage;

impl UpdateCoverage {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let phid = string(request, "repositoryPHID")?;
        let context = request.context;
        let database = &context.database;
        let found = repository::load(database, &[Phid::new(phid.clone())]).await?;
        if found.is_empty() {
            return Err(pager::core_error(format!(
                "No repository exists with PHID \"{phid}\"."
            )));
        }
        let visible = repository::visible(
            database,
            &context.config,
            &request.viewer,
            found,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        let Some(repository) = visible.into_iter().next() else {
            return Err(unsupported("repositories the viewer can't see"));
        };
        if repository.vcs != "git" {
            return Err(unsupported("repositories other than Git"));
        }
        let name = string(request, "commit")?;
        let branch = string(request, "branch")?;
        let coverage: Vec<(&str, &str)> = match request.value("coverage") {
            Some(Value::Object(coverage)) if !coverage.is_empty() => coverage
                .iter()
                .map(|(path, value)| value.as_str().map(|value| (path.as_str(), value)))
                .collect::<Option<_>>()
                .ok_or(unsupported("coverage which is not strings"))?,
            _ => return Err(unsupported("coverage which is not a nonempty map")),
        };
        let mode = mode(request)?;
        if branch.chars().count() > BRANCH_LENGTH {
            return Err(unsupported("branch names longer than their column"));
        }
        let Some(commit_id) = commit_id(request, repository.id, &name).await? else {
            return Err(pager::core_error(format!(
                "No commit exists with identifier \"{name}\"."
            )));
        };
        let paths: Vec<&str> = coverage.iter().map(|(path, _)| *path).collect();
        // The paths are checked before the branch is created, which PHP
        // does before it fails on a missing path.
        let path_ids = path_ids(request, &paths).await?;
        edit::require_writes(request)?;
        let branch_id = branch_id(request, repository.id, &branch).await?;
        let mode = mode.map_err(pager::core_error)?;

        let table = database.table("repository", "repository_coverage");
        let mut transaction = database.pool().begin().await?;
        if mode == Mode::Overwrite {
            phab_db::sqlx::query(&format!("DELETE FROM {table} WHERE branchID = ?"))
                .bind(branch_id)
                .execute(&mut *transaction)
                .await?;
        }
        let lengths: Vec<usize> = coverage
            .iter()
            .zip(&path_ids)
            .map(|((_, value), path_id)| fragment_length([branch_id, *path_id, commit_id], value))
            .collect();
        let mut start = 0;
        for size in chunk_sizes(&lengths) {
            let sql = format!(
                "INSERT INTO {table} (branchID, pathID, commitID, coverage) VALUES {} \
                 ON DUPLICATE KEY UPDATE coverage = VALUES(coverage)",
                vec!["(?, ?, ?, ?)"; size].join(", ")
            );
            let mut query = phab_db::sqlx::query(&sql);
            for index in start..start + size {
                query = query
                    .bind(branch_id)
                    .bind(path_ids[index])
                    .bind(commit_id)
                    .bind(coverage[index].1.as_bytes());
            }
            query.execute(&mut *transaction).await?;
            start += size;
        }
        transaction.commit().await?;
        Ok(Value::Null)
    }
}

impl Method for UpdateCoverage {
    fn name(&self) -> &'static str {
        "diffusion.updatecoverage"
    }

    fn params(&self) -> &'static [&'static str] {
        &["repositoryPHID", "branch", "commit", "coverage", "mode"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_fragments_like_qsprintf() {
        // "(1, 22, 333, 'NC\'U')".
        assert_eq!(fragment_length([1, 22, 333], "NC'U"), 21);
    }

    #[test]
    fn chunks_like_lisk() {
        assert_eq!(chunk_sizes(&[10, 10, 10]), [3]);
        assert_eq!(chunk_sizes(&[CHUNK_LIMIT, 10, 10]), [1, 2]);
        assert_eq!(chunk_sizes(&[CHUNK_LIMIT + 1, 10]), [1, 1]);
    }
}
