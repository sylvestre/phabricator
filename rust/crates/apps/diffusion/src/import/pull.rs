//! `PhabricatorRepositoryPullEngine::pullRepository()` for an observed Git
//! repository whose working copy exists: compares the refs of the remote
//! and of the working copy (`git ls-remote`), deletes the surplus local
//! refs and fetches, then points `HEAD` at the default branch.
//!
//! Left to PHP: hosted repositories (PHP installs their hooks), working
//! copies to create or rebuild, remotes which need credentials or SSH,
//! fetch rules (`fetch-rules`), and the shallow fetches of Mozilla's
//! `diffusion.shallow-repos`.

use std::collections::BTreeMap;
use std::time::Duration;

use phab_core::{Config, php};
use phab_db::Database;
use serde_json::json;

use crate::git::WorkingCopy;
use crate::import::ImportError;
use crate::import::lock::GlobalLock;
use crate::import::repository::{CODE_ERROR, CODE_OKAY, ImportRepository, STATUS_INIT};

/// How long a fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// `getRemoteURIObject()` as a string: an absolute path is a `file://`
/// URI. Remotes Rust can't reach as PHP does are left to PHP.
pub fn remote_uri(repository: &ImportRepository) -> Result<String, ImportError> {
    let raw = match repository.detail("remote-uri") {
        Some(serde_json::Value::String(raw)) if !raw.is_empty() => raw.clone(),
        _ => {
            return Err(ImportError::unsupported(
                "a repository without a remote URI",
            ));
        }
    };
    let uri = if raw.starts_with('/') {
        format!("file://{raw}")
    } else {
        raw
    };
    if !uri.starts_with("file://") {
        // SSH needs PHP's wrapper and key; HTTP may need credentials or a
        // proxy.
        return Err(ImportError::unsupported(
            "a remote which is not a local path",
        ));
    }
    if uri.contains(['\n', '\0']) {
        return Err(ImportError::unsupported("an unusual remote URI"));
    }
    Ok(uri)
}

/// The checks done before the pull writes anything.
pub fn check(config: &Config, repository: &ImportRepository) -> Result<(), ImportError> {
    repository.check_supported()?;
    if repository.is_hosted() {
        return Err(ImportError::unsupported(
            "a hosted repository, whose hooks PHP installs",
        ));
    }
    if repository
        .credential_phid
        .as_deref()
        .is_some_and(php::is_truthy_str)
    {
        return Err(ImportError::unsupported("a repository with a credential"));
    }
    if repository.detail("fetch-rules").is_some_and(php::is_truthy) {
        return Err(ImportError::unsupported("fetch rules"));
    }
    let shallow = config
        .object("diffusion.shallow-repos")
        .zip(repository.callsign.as_deref())
        .is_some_and(|(map, callsign)| map.contains_key(callsign));
    if shallow {
        return Err(ImportError::unsupported("a shallow repository"));
    }
    remote_uri(repository)?;
    Ok(())
}

/// `loadGitRemoteRefs()`: `git ls-remote` of `remote`, without `HEAD` and
/// the remote's own remotes, sorted by name.
async fn remote_refs(
    copy: &WorkingCopy,
    remote: &str,
    rules: &[&str],
) -> Result<BTreeMap<String, String>, ImportError> {
    let mut argv = vec!["ls-remote".to_owned(), remote.to_owned()];
    argv.extend(rules.iter().map(|rule| (*rule).to_owned()));
    let output = copy.exec_with_timeout(argv, None, FETCH_TIMEOUT).await?;
    if !output.success() {
        return Err(ImportError::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let stdout = crate::php::text(output.stdout)?;
    let mut map = BTreeMap::new();
    if crate::php::rtrim(&stdout).is_empty() {
        return Ok(map);
    }
    for line in crate::import::text::split_lines(&stdout) {
        let mut fields = line.splitn(2, |character: char| character.is_ascii_whitespace());
        let hash = fields.next().unwrap_or_default();
        let name = fields
            .next()
            .map(|name| name.trim_start_matches(|character: char| character.is_ascii_whitespace()))
            .unwrap_or_default();
        if name == "HEAD" || name.starts_with("refs/remotes/") {
            continue;
        }
        map.insert(name.to_owned(), hash.to_owned());
    }
    Ok(map)
}

/// `pullRepository()`, under the `repo.pull` lock.
pub async fn pull_repository(
    database: &Database,
    config: &Config,
    repository: &ImportRepository,
    log: &mut Vec<String>,
) -> Result<(), ImportError> {
    check(config, repository)?;
    let name = crate::import::lock::repository_lock_name(
        database.namespace(),
        "repo.pull",
        &repository.phid,
    );
    let Some(lock) = GlobalLock::try_lock(database, name).await? else {
        return Err(ImportError::Locked(format!(
            "Another process is currently updating repository \"{}\", skipping pull.",
            repository.display_name()
        )));
    };
    let mut result = pull_with_lock(database, repository, log).await;
    if let Err(ImportError::Failed(message)) = &result {
        // abortPull(): PHP's message quotes its CommandException, Rust's
        // what Git printed.
        let parameters = json!({
            "message": format!("Pull of '{}' failed: {message}", repository.display_name()),
        });
        if let Err(error) = repository
            .write_status_message(database, STATUS_INIT, Some(CODE_ERROR), &parameters)
            .await
        {
            result = Err(error.into());
        }
    }
    let unlocked = lock.unlock().await;
    result?;
    unlocked
}

async fn pull_with_lock(
    database: &Database,
    repository: &ImportRepository,
    log: &mut Vec<String>,
) -> Result<(), ImportError> {
    let okay = json!({"message": null});
    let copy = repository.working_copy()?;
    // What Git says before the first write: a broken working copy is left
    // to PHP, which destroys it and clones again.
    check_repository_root(&copy).await?;
    let remote = remote_uri(repository)?;
    let remote_refs = remote_refs(&copy, &remote, &["refs/*"]).await?;
    let path = repository.local_path.clone().unwrap_or_default();
    let mut local_refs = remote_refs_of_path(&copy, &path).await?;
    if repository.is_read_only() {
        // skipPull() reports success, and the pull goes on.
        log.push("Skipping pull on read-only repository.".to_owned());
        repository
            .write_status_message(database, STATUS_INIT, Some(CODE_OKAY), &okay)
            .await?;
    }
    log.push(format!(
        "Updating the working copy for repository \"{}\".",
        repository.display_name()
    ));
    let surplus: Vec<String> = local_refs
        .keys()
        .filter(|name| !remote_refs.contains_key(*name))
        .cloned()
        .collect();
    for name in surplus {
        log.push(format!("Deleting surplus local ref \"{name}\"."));
        // `%R`: a ref name Git could read as a flag is refused.
        let name = crate::git::ref_selector(&name)?;
        copy.exec(
            vec![
                "update-ref".to_owned(),
                "-d".to_owned(),
                name.clone(),
                "--".to_owned(),
            ],
            None,
        )
        .await?;
        local_refs.remove(&name);
    }
    if remote_refs == local_refs {
        log.push("Skipping fetch because local and remote refs are already identical.".to_owned());
    } else {
        // `git ft`: the alias of `fetch` in Mozilla's image.
        let output = copy
            .exec_with_timeout(
                vec![
                    "fetch".to_owned(),
                    "--no-tags".to_owned(),
                    "--update-head-ok".to_owned(),
                    "--".to_owned(),
                    remote,
                    "+refs/*:refs/*".to_owned(),
                ],
                None,
                FETCH_TIMEOUT,
            )
            .await?;
        if !output.success() {
            return Err(ImportError::Failed(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
    }
    // updateGitWorkingCopyConfiguration(), errors ignored.
    if !copy.path().join(".git").exists() {
        let _ = copy
            .exec(
                vec![
                    "config".to_owned(),
                    "--".to_owned(),
                    "receive.denyDeleteCurrent".to_owned(),
                    "ignore".to_owned(),
                ],
                None,
            )
            .await;
        let _ = copy
            .exec(
                vec![
                    "symbolic-ref".to_owned(),
                    "HEAD".to_owned(),
                    format!("refs/heads/{}", repository.default_branch()),
                ],
                None,
            )
            .await;
    }
    repository
        .write_status_message(database, STATUS_INIT, Some(CODE_OKAY), &okay)
        .await?;
    Ok(())
}

/// The local refs: `git ls-remote` of the working copy's own path.
async fn remote_refs_of_path(
    copy: &WorkingCopy,
    path: &str,
) -> Result<BTreeMap<String, String>, ImportError> {
    remote_refs(copy, path, &[]).await
}

/// The sanity check of `executeGitUpdate()`: the working copy must be the
/// root of a Git repository. PHP destroys and clones again a broken one
/// (or fails with a message quoting the path): left to PHP.
async fn check_repository_root(copy: &WorkingCopy) -> Result<(), ImportError> {
    let output = copy
        .exec(vec!["rev-parse".to_owned(), "--git-dir".to_owned()], None)
        .await?;
    if !output.success() {
        return Err(ImportError::unsupported(
            "a working copy which is not a Git repository",
        ));
    }
    let stdout = crate::php::text(output.stdout)?;
    let git_dir = stdout.trim_end_matches('\n');
    let resolved = copy.path().join(git_dir);
    let root = std::fs::canonicalize(&resolved).ok();
    let path = std::fs::canonicalize(copy.path()).ok();
    let is_root = root.is_some() && root == path;
    let parent_is_root = root
        .as_ref()
        .and_then(|root| root.parent())
        .map(std::path::Path::to_path_buf)
        .is_some_and(|parent| Some(parent) == path);
    if !(is_root || parent_is_root) {
        return Err(ImportError::unsupported(
            "a working copy inside another Git repository",
        ));
    }
    Ok(())
}
