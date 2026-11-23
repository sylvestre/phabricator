//! `diffusion.lastmodifiedquery` (`DiffusionLastModifiedQueryConduitAPIMethod`):
//! the commit which last changed each path (`git log -n1 --format=%H`).
//!
//! Ported for Git, with `PhabricatorRepositoryGraphCache`, which answers
//! from the imported path changes for paths in `repository_path` at
//! imported commits (and may differ from Git). Left to PHP: commits named
//! with another repository's monogram, and values of unusual types.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::{Map, Value, json};

use crate::drequest::DiffusionRequest;
use crate::graph_cache::{GraphCache, Lookup};
use crate::php::{self, Array, Key};
use crate::query::QueryMethod;
use crate::{git, history};

/// `PhabricatorRepository::MINIMUM_QUALIFIED_HASH`.
const MINIMUM_QUALIFIED_HASH: usize = 5;

/// The time `loadCommitsFromCache()` spends in the graph cache at most.
const CACHE_TIME: Duration = Duration::from_secs(10);

/// The repository part of a commit name (`^(?:[rR]([A-Z]+:?|[0-9]+:))?`),
/// and the rest.
fn split_monogram(name: &str) -> (Option<&str>, &str) {
    let Some(rest) = name.strip_prefix(['r', 'R']) else {
        return (None, name);
    };
    let letters = rest.bytes().take_while(u8::is_ascii_uppercase).count();
    if letters > 0 {
        let end = if rest[letters..].starts_with(':') {
            letters + 1
        } else {
            letters
        };
        return (Some(&rest[..letters]), &rest[end..]);
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && rest[digits..].starts_with(':') {
        return (Some(&rest[..digits]), &rest[digits + 1..]);
    }
    (None, name)
}

/// `DiffusionCommitQuery::withRepository()->withIdentifiers()` and
/// `getIdentifierMap()`: the ID of the one commit of the repository each
/// name stands for.
async fn commit_ids(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    names: &[&str],
) -> Result<HashMap<String, i64>, MethodError> {
    let database = &request.context.database;
    let mut found = HashMap::new();
    for name in names {
        let (repository, hash) = split_monogram(name);
        if hash.len() < MINIMUM_QUALIFIED_HASH {
            continue;
        }
        if let Some(repository) = repository {
            let repositories = phab_app_differential::repository::load_by_identifiers(
                database,
                &[repository.to_owned()],
            )
            .await?;
            if repositories.is_empty() {
                continue;
            }
            return Err(MethodError::Unsupported(
                "a commit named with a repository's monogram",
            ));
        }
        let (condition, value) = if hash.len() == 40 {
            ("commitIdentifier = ?", hash.to_owned())
        } else {
            (
                "commitIdentifier LIKE ?",
                format!("{}%", phab_db::like_escape(hash)),
            )
        };
        let sql = format!(
            "SELECT id FROM {} WHERE repositoryID = ? AND {condition} LIMIT 2",
            database.table("repository", "repository_commit"),
        );
        let rows = phab_db::sqlx::query(&sql)
            .bind(drequest.repository.id)
            .bind(value)
            .fetch_all(database.pool())
            .await?;
        if let [row] = rows.as_slice() {
            found.insert((*name).to_owned(), phab_db::rows::int(row, "id")?);
        }
    }
    Ok(found)
}

/// `loadCommitsFromCache()`: what the graph cache says of each path, as
/// commit identifiers.
async fn from_cache(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    paths: &[(Key, String)],
) -> Result<HashMap<Key, String>, MethodError> {
    let database = &request.context.database;
    let mut path_ids = HashMap::new();
    for (path, _) in paths {
        if let Some(id) = history::path_id(database, &history::normalize_path(&path.text())).await?
        {
            path_ids.insert(path.clone(), id);
        }
    }
    if path_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut names: Vec<&str> = paths.iter().map(|(_, commit)| commit.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let commits = commit_ids(request, drequest, &names).await?;

    let mut cache = GraphCache::default();
    let mut found: Vec<(Key, i64)> = Vec::new();
    let mut remaining = CACHE_TIME;
    for (path, commit) in paths {
        let (Some(&path_id), Some(&commit_id)) = (path_ids.get(path), commits.get(commit)) else {
            continue;
        };
        let start = Instant::now();
        let lookup = cache
            .last_modified_commit_id(request, commit_id, path_id, remaining)
            .await?;
        if let Lookup::Commit(id) = lookup {
            found.push((path.clone(), id));
        }
        match remaining.checked_sub(start.elapsed()) {
            Some(left) if !left.is_zero() => remaining = left,
            _ => break,
        }
    }
    if found.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<i64> = found.iter().map(|(_, id)| *id).collect();
    let sql = format!(
        "SELECT id, commitIdentifier FROM {} WHERE repositoryID = ? AND id IN ({})",
        database.table("repository", "repository_commit"),
        phab_db::placeholders(ids.len())
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&sql).bind(drequest.repository.id),
        ids.iter().copied(),
    )
    .fetch_all(database.pool())
    .await?;
    let mut identifiers = HashMap::new();
    for row in &rows {
        identifiers.insert(
            phab_db::rows::int(row, "id")?,
            phab_db::rows::text(row, "commitIdentifier")?,
        );
    }
    Ok(found
        .into_iter()
        .filter_map(|(path, id)| Some((path, identifiers.get(&id)?.clone())))
        .collect())
}

/// The commits which last changed `paths` (path to commit) as PHP keys
/// them.
pub async fn last_modified(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    paths: &[(Key, String)],
) -> Result<Array, MethodError> {
    // loadCommitsFromCache(): `DiffusionPathIDQuery` can't build its query
    // without paths.
    if paths.is_empty() {
        return Err(MethodError::Conduit {
            code: "ERR-CONDUIT-CORE".to_owned(),
            info: Some(
                "Array for %Ls conversion is empty. Query: SELECT * FROM %s WHERE pathHash IN \
                 (%Ls)"
                    .to_owned(),
            ),
        });
    }
    let mut cached = from_cache(request, drequest, paths).await?;
    let mut results = Array::new();
    for (path, _) in paths {
        if let Some(commit) = cached.remove(path) {
            results.set(path.clone(), json!(commit));
        }
    }
    for (path, commit) in paths {
        if results.get(path).is_some() {
            continue;
        }
        let stdout = drequest
            .copy
            .execx(
                vec![
                    "log".to_owned(),
                    "-n1".to_owned(),
                    "--format=%H".to_owned(),
                    git::ref_selector(commit)?,
                    "--".to_owned(),
                    path.text(),
                ],
                None,
            )
            .await?;
        results.set(path.clone(), json!(php::trim(&php::text(stdout)?)));
    }
    Ok(results)
}

/// The `paths` parameter, a map of paths to commits.
fn paths(value: Option<&Value>) -> Result<Vec<(Key, String)>, MethodError> {
    let map: &Map<String, Value> = match value {
        Some(Value::Object(map)) => map,
        Some(Value::Array(list)) if list.is_empty() => return Ok(Vec::new()),
        _ => return Err(MethodError::Unsupported("paths which are not a map")),
    };
    let mut paths: Vec<(Key, String)> = Vec::new();
    for (path, commit) in map {
        let Value::String(commit) = commit else {
            return Err(MethodError::Unsupported("commits which are not strings"));
        };
        let key = Key::from_text(path);
        match paths.iter_mut().find(|(known, _)| *known == key) {
            Some(entry) => entry.1.clone_from(commit),
            None => paths.push((key, commit.clone())),
        }
    }
    Ok(paths)
}

/// `diffusion.lastmodifiedquery`.
pub struct LastModifiedQuery;

impl QueryMethod for LastModifiedQuery {
    fn name(&self) -> &'static str {
        "diffusion.lastmodifiedquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &["paths", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let paths = paths(request.value("paths"))?;
            Ok(last_modified(request, drequest, &paths).await?.into_json())
        })
    }
}
