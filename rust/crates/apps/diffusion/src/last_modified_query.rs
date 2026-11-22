//! `diffusion.lastmodifiedquery` (`DiffusionLastModifiedQueryConduitAPIMethod`):
//! the commit which last changed each path (`git log -n1 --format=%H`).
//!
//! Ported for Git: paths the graph cache can't answer for. PHP first asks
//! `PhabricatorRepositoryGraphCache` about paths in `repository_path` at
//! imported commits, whose answer comes from the imported path changes
//! and may differ from Git's; such calls are left to PHP, as are values of
//! unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::{Map, Value, json};

use crate::drequest::DiffusionRequest;
use crate::php::{self, Array, Key};
use crate::query::QueryMethod;
use crate::{git, history};

/// Whether the repository has imported commits, which the graph cache
/// could answer from.
async fn has_commits(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
) -> Result<bool, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE repositoryID = ? LIMIT 1",
        database.table("repository", "repository_commit")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(drequest.repository.id)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// The commits which last changed `paths` (path to commit) as PHP keys
/// them.
pub async fn last_modified(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    paths: &[(Key, String)],
) -> Result<Array, MethodError> {
    let database = &request.context.database;
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
    let mut known_path = false;
    for (path, _) in paths {
        if history::path_id(database, &history::normalize_path(&path.text()))
            .await?
            .is_some()
        {
            known_path = true;
            break;
        }
    }
    if known_path && has_commits(request, drequest).await? {
        return Err(MethodError::Unsupported(
            "paths the graph cache may answer for",
        ));
    }
    let mut results = Array::new();
    for (path, commit) in paths {
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
