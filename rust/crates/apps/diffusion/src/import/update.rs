//! The `bin/repository` workflows of the pipeline:
//! `PhabricatorRepositoryManagementUpdateWorkflow` (pull, discover, refs;
//! what `PhabricatorRepositoryPullLocalDaemon` runs for each repository),
//! `DiscoverWorkflow` and `RefsWorkflow`.

use phab_core::Config;
use phab_db::Database;
use serde_json::json;

use crate::import::discovery::DiscoveryEngine;
use crate::import::ref_engine::RefEngine;
use crate::import::repository::{
    CODE_ERROR, CODE_OKAY, ImportRepository, STATUS_FETCH, STATUS_NEEDS_UPDATE,
};
use crate::import::{IMPORTED_ALL, IMPORTED_UNREACHABLE, ImportError, check_config, pull};

/// How a workflow ended, when it did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What it prints.
    pub output: Vec<String>,
    /// The lines of `--verbose`.
    pub log: Vec<String>,
}

/// The mirrors a repository pushes to after an update, which are left to
/// PHP.
async fn has_mirrors(
    database: &Database,
    repository: &ImportRepository,
) -> Result<bool, ImportError> {
    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE repositoryPHID = ? AND ioType = 'mirror' AND \
         isDisabled = 0",
        database.table("repository", "repository_uri")
    ))
    .bind(&repository.phid)
    .fetch_one(database.pool())
    .await?;
    Ok(count > 0)
}

/// `bin/repository discover [--repair] <repository>`.
pub async fn discover(
    database: &Database,
    config: &Config,
    repository: &mut ImportRepository,
    repair: bool,
) -> Result<Outcome, ImportError> {
    let mut output = vec![format!("Discovering \"{}\"...", repository.display_name())];
    let mut engine = DiscoveryEngine::new(database, repository, repair);
    engine.discover_commits(config).await?;
    let log = engine.log().to_vec();
    output.push("Done.".to_owned());
    Ok(Outcome { output, log })
}

/// `bin/repository refs <repository>`.
pub async fn refs(
    database: &Database,
    config: &Config,
    repository: &ImportRepository,
) -> Result<Outcome, ImportError> {
    check_config(config)?;
    let mut engine = RefEngine::new(database, repository)?;
    engine.update_refs().await?;
    Ok(Outcome {
        output: vec![
            format!("Updating refs in \"{}\"...", repository.display_name()),
            "Done.".to_owned(),
        ],
        log: engine.log().to_vec(),
    })
}

/// The checks of `bin/repository update` done before anything is written.
pub async fn check_update(
    database: &Database,
    config: &Config,
    repository: &ImportRepository,
) -> Result<(), ImportError> {
    DiscoveryEngine::check(config, repository)?;
    pull::check(config, repository)?;
    if has_mirrors(database, repository).await? {
        return Err(ImportError::unsupported("a repository with mirrors"));
    }
    Ok(())
}

/// `bin/repository update [--no-discovery] <repository>`.
pub async fn update(
    database: &Database,
    config: &Config,
    repository: &mut ImportRepository,
    no_discovery: bool,
) -> Result<Outcome, ImportError> {
    check_update(database, config, repository).await?;
    let mut log = Vec::new();
    match update_steps(database, config, repository, no_discovery, &mut log).await {
        Ok(Some(())) => Ok(Outcome {
            output: vec![format!(
                "Updated repository \"{}\".",
                repository.display_name()
            )],
            log,
        }),
        Ok(None) => Ok(Outcome {
            output: Vec::new(),
            log,
        }),
        // DiffusionDaemonLockException: another process does the work.
        Err(ImportError::Locked(message)) => Ok(Outcome {
            output: vec![message],
            log,
        }),
        Err(ImportError::Failed(message)) => {
            // The exception's message differs from PHP's when it quotes a
            // CommandException.
            repository
                .write_status_message(
                    database,
                    STATUS_FETCH,
                    Some(CODE_ERROR),
                    &json!({"message": format!("Error updating working copy: {message}")}),
                )
                .await?;
            Err(ImportError::Failed(message))
        }
        Err(error) => Err(error),
    }
}

async fn update_steps(
    database: &Database,
    config: &Config,
    repository: &mut ImportRepository,
    no_discovery: bool,
    log: &mut Vec<String>,
) -> Result<Option<()>, ImportError> {
    pull::pull_repository(database, config, repository, log).await?;
    if no_discovery {
        return Ok(None);
    }
    repository
        .write_status_message(database, STATUS_NEEDS_UPDATE, None, &json!([]))
        .await?;
    {
        let mut engine = DiscoveryEngine::new(database, repository, false);
        engine.discover_commits(config).await?;
        log.extend(engine.log().iter().cloned());
    }
    check_if_fully_imported(database, repository).await?;
    {
        let mut engine = RefEngine::new(database, repository)?;
        engine.update_refs().await?;
        log.extend(engine.log().iter().cloned());
    }
    // pushToMirrors(): none, checked first.
    repository
        .write_status_message(database, STATUS_FETCH, Some(CODE_OKAY), &json!([]))
        .await?;
    Ok(Some(()))
}

/// `checkIfRepositoryIsFullyImported()`: clears `importing` once every
/// reachable commit is imported.
async fn check_if_fully_imported(
    database: &Database,
    repository: &mut ImportRepository,
) -> Result<(), ImportError> {
    if !repository.is_importing() {
        return Ok(());
    }
    let unparsed: Option<u32> = sqlx::query_scalar(&format!(
        "SELECT id FROM {} WHERE repositoryID = ? AND (importStatus & ?) != ? AND \
         (importStatus & ?) != ? LIMIT 1",
        database.table("repository", "repository_commit")
    ))
    .bind(repository.id)
    .bind(IMPORTED_ALL)
    .bind(IMPORTED_ALL)
    .bind(IMPORTED_UNREACHABLE)
    .bind(IMPORTED_UNREACHABLE)
    .fetch_optional(database.pool())
    .await?;
    if unparsed.is_some() {
        return Ok(());
    }
    repository
        .details
        .insert("importing".to_owned(), serde_json::Value::Bool(false));
    repository.save_details(database).await?;
    Ok(())
}
