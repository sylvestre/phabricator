//! `PhabricatorRepositoryDiscoveryEngine::discoverCommits()` for Git: finds
//! the commits of the tracked refs which are not in the database yet,
//! records them (`repository_commit`, `repository_commitdata`,
//! `repository_parents`, `repository_summary`) with a message parser task
//! each, and marks the commits of deleted refs unreachable.

use std::collections::HashSet;

use indexmap::IndexMap;
use phab_core::{Config, Phid, PhidType};
use phab_db::{Database, bind_all, placeholders, rows};

use crate::import::commit::Commit;
use crate::import::graph::{GitGraphStream, topological_order};
use crate::import::lock::GlobalLock;
use crate::import::repository::ImportRepository;
use crate::import::{
    IMPORT_THRESHOLD, IMPORTED_PERMANENT, IMPORTED_UNREACHABLE, ImportError, check_config,
    import_task_priority, now, queue_commit_import_task,
};
use crate::refs::{self, BRANCH, REF, TAG};

/// `MAX_COMMIT_CACHE_SIZE`.
const MAX_COMMIT_CACHE_SIZE: usize = 65535;

/// The identifiers of one query: MySQL takes at most 65535 placeholders.
const QUERY_CHUNK: usize = 4096;

/// `PhabricatorRepositoryCommitRef`: a discovered commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef {
    /// The hash.
    pub identifier: String,
    /// The commit date.
    pub epoch: i64,
    /// Whether a permanent ref has it.
    pub is_permanent: bool,
    /// Its parents, without duplicates.
    pub parents: Vec<String>,
}

/// The discovery engine of one repository.
pub struct DiscoveryEngine<'a> {
    database: &'a Database,
    repository: &'a mut ImportRepository,
    repair: bool,
    commit_cache: IndexMap<String, ()>,
    working_set: HashSet<String>,
    log: Vec<String>,
}

impl<'a> DiscoveryEngine<'a> {
    /// The engine; `repair` rediscovers commits the database already has
    /// (`--repair`).
    pub fn new(database: &'a Database, repository: &'a mut ImportRepository, repair: bool) -> Self {
        Self {
            database,
            repository,
            repair,
            commit_cache: IndexMap::new(),
            working_set: HashSet::new(),
            log: Vec::new(),
        }
    }

    /// What `--verbose` would print.
    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// The checks done before anything is written.
    pub fn check(config: &Config, repository: &ImportRepository) -> Result<(), ImportError> {
        check_config(config)?;
        repository.check_supported()?;
        repository.check_filters()
    }

    /// `discoverCommits()`, under the `repo.look` lock.
    pub async fn discover_commits(
        &mut self,
        config: &Config,
    ) -> Result<Vec<CommitRef>, ImportError> {
        Self::check(config, self.repository)?;
        let name = crate::import::lock::repository_lock_name(
            self.database.namespace(),
            "repo.look",
            &self.repository.phid,
        );
        let Some(lock) = GlobalLock::try_lock(self.database, name).await? else {
            return Err(ImportError::Locked(format!(
                "Another process is currently discovering repository \"{}\", skipping discovery.",
                self.repository.display_name()
            )));
        };
        let result = self.discover_with_lock().await;
        let unlocked = lock.unlock().await;
        let refs = result?;
        unlocked?;
        Ok(refs)
    }

    async fn discover_with_lock(&mut self) -> Result<Vec<CommitRef>, ImportError> {
        let refs = self.discover_git_commits().await?;
        if self.is_initial_import(&refs).await? {
            self.log.push(format!(
                "Discovered more than {IMPORT_THRESHOLD} commit(s) in an empty repository, \
                 marking repository as importing."
            ));
            self.repository.mark_importing(self.database).await?;
        }
        self.working_set.clear();
        let priority = import_task_priority(self.repository, refs.len());
        for commit_ref in &refs {
            self.record_commit(commit_ref, priority).await?;
            self.commit_cache.insert(commit_ref.identifier.clone(), ());
        }
        self.mark_unreachable_commits().await?;
        // getObservedVersion() only matters to a cluster.
        Ok(refs)
    }

    async fn discover_git_commits(&mut self) -> Result<Vec<CommitRef>, ImportError> {
        let copy = self.repository.working_copy()?;
        let heads = refs::git_refs(&copy, &[BRANCH, TAG, REF]).await?;
        if heads.is_empty() {
            return Ok(Vec::new());
        }
        self.log.push(format!(
            "Discovering commits in repository \"{}\".",
            self.repository.display_name()
        ));
        // sortRefs(): permanent refs first.
        let mut sorted = Vec::with_capacity(heads.len());
        let mut tail = Vec::new();
        for head in heads {
            if self
                .repository
                .is_permanent_ref(head.ref_type, &head.short_name)?
            {
                sorted.push(head);
            } else {
                tail.push(head);
            }
        }
        sorted.extend(tail);
        let mut discovered = Vec::new();
        for group in sorted.chunks(MAX_COMMIT_CACHE_SIZE) {
            let mut identifiers: IndexMap<String, ()> = IndexMap::new();
            for head in group {
                identifiers.insert(head.commit.clone(), ());
            }
            let identifiers: Vec<String> = identifiers.into_keys().collect();
            self.fill_commit_cache(&identifiers).await?;
            for head in group {
                self.log.push(format!(
                    "Examining \"{}\" ({}) at \"{}\".",
                    head.short_name, head.ref_type, head.commit
                ));
                if !self.repository.should_track_ref(head)? {
                    self.log.push("Skipping, ref is untracked.".to_owned());
                    continue;
                }
                if self.is_known_commit(&head.commit).await? {
                    self.log.push("Skipping, HEAD is known.".to_owned());
                    continue;
                }
                // Tags may point at anything but commits.
                if head.field("objecttype") != Some("commit")
                    && head.field("*objecttype") != Some("commit")
                {
                    self.log.push("Skipping, this is not a commit.".to_owned());
                    continue;
                }
                self.log.push("Looking for new commits.".to_owned());
                let is_permanent = self
                    .repository
                    .is_permanent_ref(head.ref_type, &head.short_name)?;
                let mut stream = GitGraphStream::new(&copy, Some(&head.commit))?;
                let head_refs = self
                    .discover_stream_ancestry(&mut stream, &head.commit, is_permanent)
                    .await?;
                for commit_ref in &head_refs {
                    self.working_set.insert(commit_ref.identifier.clone());
                }
                discovered.extend(head_refs);
            }
        }
        Ok(discovered)
    }

    async fn discover_stream_ancestry(
        &mut self,
        stream: &mut GitGraphStream,
        commit: &str,
        is_permanent: bool,
    ) -> Result<Vec<CommitRef>, ImportError> {
        let mut discover = vec![commit.to_owned()];
        let mut graph: IndexMap<String, Vec<String>> = IndexMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(target) = discover.pop() {
            if graph.get(&target).is_none_or(Vec::is_empty) {
                graph.insert(target.clone(), Vec::new());
            }
            for parent in stream.parents(&target).await? {
                if self.is_known_commit(&parent).await? {
                    continue;
                }
                let edges = graph.entry(target.clone()).or_default();
                if !edges.contains(&parent) {
                    edges.push(parent.clone());
                }
                if seen.insert(parent.clone()) {
                    discover.push(parent);
                }
            }
        }
        let mut commits = topological_order(&graph);
        commits.reverse();
        let mut refs = Vec::with_capacity(commits.len());
        for commit in commits {
            let date = stream.commit_date(&commit).await?;
            let epoch = if date.is_empty() {
                0
            } else {
                // A date which doesn't fit a uint32 is now (T11537).
                match date.parse::<i64>() {
                    Ok(epoch) if epoch <= 0xffff_ffff => epoch,
                    Ok(_) => now(),
                    Err(_) => {
                        return Err(ImportError::unsupported("an unusual commit date"));
                    }
                }
            };
            refs.push(CommitRef {
                identifier: commit.clone(),
                epoch,
                is_permanent,
                parents: stream.parents(&commit).await?,
            });
        }
        Ok(refs)
    }

    async fn is_known_commit(&mut self, identifier: &str) -> Result<bool, ImportError> {
        if self.commit_cache.contains_key(identifier) || self.working_set.contains(identifier) {
            return Ok(true);
        }
        self.fill_commit_cache(&[identifier.to_owned()]).await?;
        Ok(self.commit_cache.contains_key(identifier))
    }

    /// `fillCommitCache()`: the reachable commits of the database among
    /// `identifiers` (repair mode trusts none).
    async fn fill_commit_cache(&mut self, identifiers: &[String]) -> Result<(), ImportError> {
        if identifiers.is_empty() || self.repair {
            return Ok(());
        }
        let identifiers = &identifiers[..identifiers.len().min(MAX_COMMIT_CACHE_SIZE)];
        for chunk in identifiers.chunks(QUERY_CHUNK) {
            let sql = format!(
                "SELECT commitIdentifier FROM {} WHERE repositoryID = ? AND commitIdentifier IN \
                 ({}) AND (importStatus & ?) != ?",
                self.database.table("repository", "repository_commit"),
                placeholders(chunk.len())
            );
            let query = bind_all(sqlx::query(&sql).bind(self.repository.id), chunk)
                .bind(IMPORTED_UNREACHABLE)
                .bind(IMPORTED_UNREACHABLE);
            for row in query.fetch_all(self.database.pool()).await? {
                self.commit_cache
                    .insert(rows::text(&row, "commitIdentifier")?, ());
            }
        }
        let excess = self
            .commit_cache
            .len()
            .saturating_sub(MAX_COMMIT_CACHE_SIZE);
        if excess > 0 {
            self.commit_cache.drain(..excess);
        }
        Ok(())
    }

    /// `isInitialImport()`.
    async fn is_initial_import(&self, refs: &[CommitRef]) -> Result<bool, ImportError> {
        if refs.len() <= IMPORT_THRESHOLD {
            return Ok(false);
        }
        let any: Option<u32> = sqlx::query_scalar(&format!(
            "SELECT id FROM {} WHERE repositoryID = ? LIMIT 1",
            self.database.table("repository", "repository_commit")
        ))
        .bind(self.repository.id)
        .fetch_optional(self.database.pool())
        .await?;
        Ok(any.is_none())
    }

    /// `recordCommit()`.
    #[allow(clippy::too_many_lines)]
    async fn record_commit(
        &mut self,
        commit_ref: &CommitRef,
        priority: i64,
    ) -> Result<(), ImportError> {
        let database = self.database;
        let table = database.table("repository", "repository_commit");
        // Revive the commit if it was discovered before.
        let revived = sqlx::query(&format!(
            "UPDATE {table} SET importStatus = (importStatus & ~?) WHERE repositoryID = ? AND \
             commitIdentifier = ? AND (importStatus & ?) = ?"
        ))
        .bind(IMPORTED_UNREACHABLE)
        .bind(self.repository.id)
        .bind(&commit_ref.identifier)
        .bind(IMPORTED_UNREACHABLE)
        .bind(IMPORTED_UNREACHABLE)
        .execute(database.pool())
        .await?
        .rows_affected();
        if revived > 0 {
            let commit =
                Commit::load_by_identifier(database, self.repository.id, &commit_ref.identifier)
                    .await?
                    .ok_or_else(|| ImportError::Failed("A revived commit vanished.".to_owned()))?;
            self.did_discover_commit(commit.id, &commit.phid, commit_ref.epoch, priority)
                .await?;
            return Ok(());
        }
        let mut parent_ids = Vec::new();
        if commit_ref.parents.is_empty() {
            // An explicit 0: "no parents", not "unknown".
            parent_ids.push(0);
        } else {
            let sql = format!(
                "SELECT id, commitIdentifier FROM {table} WHERE commitIdentifier IN ({}) AND \
                 repositoryID = ?",
                placeholders(commit_ref.parents.len())
            );
            let found = bind_all(sqlx::query(&sql), &commit_ref.parents)
                .bind(self.repository.id)
                .fetch_all(database.pool())
                .await?;
            let mut map = std::collections::HashMap::new();
            for row in &found {
                map.insert(rows::text(row, "commitIdentifier")?, rows::int(row, "id")?);
            }
            for parent in &commit_ref.parents {
                let Some(id) = map.get(parent) else {
                    return Err(ImportError::Failed(format!(
                        "Unable to identify parent \"{parent}\"!"
                    )));
                };
                parent_ids.push(*id);
            }
        }
        let phid = Phid::generate(PhidType::COMMIT, None).into_string();
        let import_status = if commit_ref.is_permanent {
            IMPORTED_PERMANENT
        } else {
            0
        };
        let mut transaction = database.pool().begin().await?;
        let inserted = sqlx::query(&format!(
            "INSERT INTO {table} (repositoryID, phid, authorIdentityPHID, committerIdentityPHID, \
             commitIdentifier, epoch, authorPHID, auditStatus, summary, importStatus) VALUES \
             (?, ?, NULL, NULL, ?, ?, NULL, 'none', '', ?)"
        ))
        .bind(self.repository.id)
        .bind(&phid)
        .bind(&commit_ref.identifier)
        .bind(commit_ref.epoch)
        .bind(import_status)
        .execute(&mut *transaction)
        .await;
        let commit_id = match inserted {
            Ok(done) => i64::try_from(done.last_insert_id()).unwrap_or_default(),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                // Discovered twice, or by another process: nothing to do.
                transaction.rollback().await?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        sqlx::query(&format!(
            "INSERT INTO {} (commitID, authorName, commitMessage, commitDetails) VALUES \
             (?, '', '', '[]')",
            database.table("repository", "repository_commitdata")
        ))
        .bind(commit_id)
        .execute(&mut *transaction)
        .await?;
        for parent_id in parent_ids {
            sqlx::query(&format!(
                "INSERT IGNORE INTO {} (childCommitID, parentCommitID) VALUES (?, ?)",
                database.table("repository", "repository_parents")
            ))
            .bind(commit_id)
            .bind(parent_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.did_discover_commit(commit_id, &phid, commit_ref.epoch, priority)
            .await?;
        if self.repair {
            self.log
                .push(format!("Repaired commit \"{}\".", commit_ref.identifier));
        }
        Ok(())
    }

    /// `didDiscoverCommit()`: the import task, then the summary.
    async fn did_discover_commit(
        &self,
        commit_id: i64,
        commit_phid: &str,
        epoch: i64,
        priority: i64,
    ) -> Result<(), ImportError> {
        queue_commit_import_task(
            self.database,
            self.repository,
            commit_phid,
            priority,
            "discovery",
        )
        .await?;
        sqlx::query(&format!(
            "INSERT INTO {} (repositoryID, size, lastCommitID, epoch) VALUES (?, 1, ?, ?) ON \
             DUPLICATE KEY UPDATE size = size + 1, lastCommitID = IF(VALUES(epoch) > epoch, \
             VALUES(lastCommitID), lastCommitID), epoch = IF(VALUES(epoch) > epoch, \
             VALUES(epoch), epoch)",
            self.database.table("repository", "repository_summary")
        ))
        .bind(self.repository.id)
        .bind(commit_id)
        .bind(epoch)
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    /// `markUnreachableCommits()`: the commits of the refs the ref engine
    /// saw go (`repository_oldref`) which Git no longer has.
    async fn mark_unreachable_commits(&mut self) -> Result<(), ImportError> {
        let database = self.database;
        let table = database.table("repository", "repository_oldref");
        let old_refs = sqlx::query(&format!(
            "SELECT id, commitIdentifier FROM {table} WHERE repositoryPHID = ? ORDER BY id"
        ))
        .bind(&self.repository.phid)
        .fetch_all(database.pool())
        .await?;
        if old_refs.is_empty() {
            return Ok(());
        }
        let copy = self.repository.working_copy()?;
        let mut stream = GitGraphStream::new(&copy, None)?;
        for old_ref in &old_refs {
            let identifier = rows::text(old_ref, "commitIdentifier")?;
            self.mark_unreachable_from(&mut stream, &identifier).await?;
            sqlx::query(&format!("DELETE FROM {table} WHERE id = ?"))
                .bind(rows::int(old_ref, "id")?)
                .execute(database.pool())
                .await?;
        }
        Ok(())
    }

    async fn mark_unreachable_from(
        &mut self,
        stream: &mut GitGraphStream,
        identifier: &str,
    ) -> Result<(), ImportError> {
        let database = self.database;
        let Some(commit) =
            Commit::load_by_identifier(database, self.repository.id, identifier).await?
        else {
            return Ok(());
        };
        let mut look = vec![commit];
        let mut seen = HashSet::new();
        let mut unreachable = Vec::new();
        while let Some(target) = look.pop() {
            if !seen.insert(target.identifier.clone()) {
                continue;
            }
            if target.is_unreachable() {
                continue;
            }
            match stream.commit_date(&target.identifier).await {
                Ok(_) => continue,
                Err(ImportError::Unsupported(reason)) => {
                    return Err(ImportError::Unsupported(reason));
                }
                Err(ImportError::Database(error)) => return Err(ImportError::Database(error)),
                Err(_) => {}
            }
            let parents = target.load_parents(database).await?;
            unreachable.push(target);
            look.extend(parents);
        }
        unreachable.reverse();
        let had_unreachable = !unreachable.is_empty();
        for mut commit in unreachable {
            commit
                .write_import_status_flag(database, IMPORTED_UNREACHABLE)
                .await?;
        }
        if had_unreachable {
            self.rebuild_summary_table().await?;
        }
        Ok(())
    }

    /// `rebuildSummaryTable()`: `%d` writes 0 for the NULLs of an empty
    /// repository.
    async fn rebuild_summary_table(&self) -> Result<(), ImportError> {
        let database = self.database;
        let row = sqlx::query(&format!(
            "SELECT COUNT(*) N, MAX(id) id, MAX(epoch) epoch FROM {} WHERE repositoryID = ? AND \
             (importStatus & ?) != ?",
            database.table("repository", "repository_commit")
        ))
        .bind(self.repository.id)
        .bind(IMPORTED_UNREACHABLE)
        .bind(IMPORTED_UNREACHABLE)
        .fetch_one(database.pool())
        .await?;
        let count = rows::opt_int(&row, "N")?.unwrap_or_default();
        let id = rows::opt_int(&row, "id")?.unwrap_or_default();
        let epoch = rows::opt_int(&row, "epoch")?.unwrap_or_default();
        sqlx::query(&format!(
            "INSERT INTO {} (repositoryID, size, lastCommitID, epoch) VALUES (?, ?, ?, ?) ON \
             DUPLICATE KEY UPDATE size = VALUES(size), lastCommitID = VALUES(lastCommitID), \
             epoch = VALUES(epoch)",
            database.table("repository", "repository_summary")
        ))
        .bind(self.repository.id)
        .bind(count)
        .bind(id)
        .bind(epoch)
        .execute(database.pool())
        .await?;
        Ok(())
    }
}
