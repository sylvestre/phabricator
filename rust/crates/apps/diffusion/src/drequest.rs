//! `DiffusionRequest` for the Git query methods
//! (`DiffusionQueryConduitAPIMethod`): the repository a call names, its
//! working copy, and the branch, path and commit of the call.
//!
//! Left to PHP: repositories the viewer can't see (`executeOne()` raises a
//! policy exception), repositories served by an Almanac service (the call
//! is proxied to a cluster node; `isClusterRequest` is always false here,
//! since cluster-signed calls never reach Rust), and Mercurial and
//! Subversion repositories.

use phab_app_differential::repository::{self, Repository};
use phab_conduit::{ApiRequest, MethodError, pager};
use phab_core::php as core_php;
use phab_db::rows;
use serde_json::Value;

use crate::git::WorkingCopy;
use crate::php::{self, Key};
use crate::refs::{self, BRANCH};

/// The error types of `DiffusionQueryConduitAPIMethod`.
pub const QUERY_ERRORS: &[(&str, &str)] = &[
    ("ERR-UNKNOWN-REPOSITORY", "There is no matching repository."),
    ("ERR-UNKNOWN-VCS-TYPE", "Unknown repository VCS type."),
    (
        "ERR-UNSUPPORTED-VCS",
        "VCS is not supported for this method.",
    ),
];

/// A request on a Git repository.
pub struct DiffusionRequest {
    /// The repository.
    pub repository: Repository,
    /// Its working copy.
    pub copy: WorkingCopy,
    branch: Option<Value>,
    path: Option<Value>,
    commit: Option<Value>,
}

impl DiffusionRequest {
    /// `newConduitCallProxyClient()`: the repository of `repository` (or
    /// the deprecated `callsign`), with the call's `branch`, `path` and
    /// `commit`.
    pub async fn from_query(request: &ApiRequest<'_>) -> Result<Self, MethodError> {
        let identifier = match request.value("repository") {
            None | Some(Value::Null) => request.value("callsign"),
            some => some,
        };
        match Self::load(request, identifier, request.value("path")).await? {
            Some(drequest) => Ok(drequest),
            None => Err(pager::core_error(format!(
                "Repository \"{}\" is not a valid repository.",
                core_php::string_cast(identifier.unwrap_or(&Value::Null))
            ))),
        }
    }

    /// `DiffusionRequest::newFromDictionary()` with `identifier`; `None`
    /// when no repository has it.
    pub async fn load(
        request: &ApiRequest<'_>,
        identifier: Option<&Value>,
        path: Option<&Value>,
    ) -> Result<Option<Self>, MethodError> {
        let identifier = match identifier {
            None | Some(Value::Null) => {
                return Err(pager::core_error(
                    "One of \"repository\" and \"callsign\" is required.".to_owned(),
                ));
            }
            Some(Value::String(identifier)) if !identifier.is_empty() => identifier,
            Some(_) => {
                return Err(MethodError::Unsupported(
                    "a repository identifier which is not a string",
                ));
            }
        };
        let context = request.context;
        let database = &context.database;
        let found =
            repository::load_by_identifiers(database, std::slice::from_ref(identifier)).await?;
        let Some(found) = found.get(identifier).cloned() else {
            return Ok(None);
        };
        let visible = repository::visible(
            database,
            &context.config,
            &request.viewer,
            vec![found],
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        let Some(repository) = visible.into_iter().next() else {
            return Err(MethodError::Unsupported(
                "a repository the viewer can't see",
            ));
        };
        let sql = format!(
            "SELECT localPath, almanacServicePHID FROM {} WHERE id = ?",
            database.table("repository", "repository")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(repository.id)
            .fetch_one(database.pool())
            .await?;
        if rows::opt_text(&row, "almanacServicePHID")?.is_some_and(|phid| !phid.is_empty()) {
            return Err(MethodError::Unsupported("calls proxied to a cluster"));
        }
        if repository.vcs != "git" {
            return Err(MethodError::Unsupported(
                "Mercurial and Subversion repositories",
            ));
        }
        let local_path = rows::opt_text(&row, "localPath")?.unwrap_or_default();
        Ok(Some(Self {
            repository,
            copy: WorkingCopy::new(local_path),
            branch: request.value("branch").cloned(),
            path: path.cloned(),
            commit: request.value("commit").cloned(),
        }))
    }

    /// `DiffusionGitRequest::getBranch()`: the branch of the call, or the
    /// default branch.
    pub fn branch(&self) -> Result<String, MethodError> {
        match &self.branch {
            Some(branch) if core_php::is_truthy(branch) => php::string(Some(branch)),
            _ => Ok(self.repository.default_branch().unwrap_or_default()),
        }
    }

    /// `getPath()`, which must be a string.
    pub fn path(&self) -> Result<String, MethodError> {
        Ok(php::optional_string(self.path.as_ref())?.unwrap_or_default())
    }

    /// `getSymbolicCommit()`.
    pub fn symbolic_commit(&self) -> Option<&Value> {
        self.commit.as_ref().filter(|commit| !commit.is_null())
    }

    /// `getCommit()`: the symbolic commit, or the stable one.
    pub async fn commit(&self, request: &ApiRequest<'_>) -> Result<String, MethodError> {
        match self.symbolic_commit() {
            Some(commit) => php::string(Some(commit)),
            None => self.stable_commit(request).await,
        }
    }

    /// `getStableCommit()`: a full hash as it is, anything else resolved
    /// (the branch when there is no commit).
    pub async fn stable_commit(&self, request: &ApiRequest<'_>) -> Result<String, MethodError> {
        let symbolic = php::optional_string(self.symbolic_commit())?;
        if let Some(symbolic) = &symbolic
            && symbolic.len() == 40
            && symbolic
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'f' | b'0'..=b'9'))
        {
            return Ok(symbolic.clone());
        }
        // queryStableCommit().
        let (reference, types) = match symbolic.filter(|symbolic| core_php::is_truthy_str(symbolic))
        {
            Some(symbolic) => (symbolic, Vec::new()),
            None => (self.branch()?, vec![BRANCH.to_owned()]),
        };
        let mut results = self
            .resolve_refs(request, std::slice::from_ref(&reference), &types)
            .await?;
        let matches = results
            .shift_remove(&Key::from_text(&reference))
            .unwrap_or_default();
        let matches: Vec<&Value> = matches.iter().map(|(_, info)| info).collect();
        let best = match matches.as_slice() {
            [] => {
                return Err(pager::core_error(format!(
                    "Ref \"{reference}\" does not exist in this repository."
                )));
            }
            [only] => *only,
            // chooseBestRefMatch(): the first open one.
            several => several
                .iter()
                .find(|info| {
                    !(info.get("type").and_then(Value::as_str) == Some(BRANCH)
                        && info.get("closed").is_some_and(core_php::is_truthy))
                })
                .copied()
                .unwrap_or(several[0]),
        };
        match best.get("identifier") {
            Some(Value::String(identifier)) => Ok(identifier.clone()),
            _ => Err(MethodError::internal("a ref without an identifier")),
        }
    }

    /// `DiffusionRequest::resolveRefs()`: from the database caches, then
    /// with `diffusion.resolverefs` for the rest.
    pub async fn resolve_refs(
        &self,
        request: &ApiRequest<'_>,
        references: &[String],
        types: &[String],
    ) -> Result<refs::Resolved, MethodError> {
        let types = (!types.is_empty()).then_some(types);
        let cached = refs::resolve_cached(
            &request.context.database,
            &self.repository,
            references,
            types,
        )
        .await?;
        let unresolved: Vec<String> = references
            .iter()
            .filter(|reference| !cached.contains_key(&Key::from_text(reference)))
            .cloned()
            .collect();
        let mut results = if unresolved.is_empty() {
            refs::Resolved::new()
        } else {
            refs::resolve(&self.copy, &unresolved, types).await?
        };
        // $vcs_results + $cached_results.
        for (key, matches) in cached {
            results.entry(key).or_insert(matches);
        }
        Ok(results)
    }
}
