//! `PhabricatorRepositoryRefEngine::updateRefs()` for Git: moves the ref
//! cursors (`repository_refcursor`, `repository_refposition`) to where the
//! refs are now, keeps the old positions for the discovery engine
//! (`repository_oldref`), and marks the commits new on permanent refs
//! `IMPORTED_PERMANENT`, with an import task each.

use indexmap::IndexMap;
use phab_core::hash::digest_for_index;
use phab_core::{Phid, PhidType};
use phab_db::{Database, bind_all, placeholders, rows};

use crate::git::{WorkingCopy, ref_selector};
use crate::import::repository::ImportRepository;
use crate::import::{
    IMPORTED_PERMANENT, IMPORTED_PUBLISH, ImportError, import_task_priority,
    queue_commit_import_task,
};
use crate::refs::{self, BRANCH, GitRef, REF, TAG};

/// `PhabricatorRepositoryRefCursorPHIDType`.
const REF_CURSOR: PhidType = PhidType::new(*b"RREF");

/// `PhabricatorRepositoryRefCursor::TYPE_BOOKMARK`, which Git never has.
const BOOKMARK: &str = "bookmark";

/// The identifiers of one query.
const QUERY_CHUNK: usize = 4096;

/// How long a `git log` of every commit of a branch may take.
const LOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// A row of `repository_refcursor`, with its positions.
#[derive(Debug, Clone)]
struct Cursor {
    id: i64,
    ref_type: String,
    name: String,
    is_permanent: bool,
    /// `(id, commitIdentifier)`.
    positions: Vec<(i64, String)>,
}

/// The ref engine of one repository.
pub struct RefEngine<'a> {
    database: &'a Database,
    repository: &'a ImportRepository,
    copy: WorkingCopy,
    new_positions: Vec<(i64, String)>,
    dead_positions: Vec<(i64, String)>,
    permanent_commits: IndexMap<String, ()>,
    log: Vec<String>,
}

impl<'a> RefEngine<'a> {
    /// The engine; the repository must be supported
    /// ([`ImportRepository::check_supported()`]).
    pub fn new(
        database: &'a Database,
        repository: &'a ImportRepository,
    ) -> Result<Self, ImportError> {
        repository.check_supported()?;
        repository.check_filters()?;
        Ok(Self {
            database,
            repository,
            copy: repository.working_copy()?,
            new_positions: Vec::new(),
            dead_positions: Vec::new(),
            permanent_commits: IndexMap::new(),
            log: Vec::new(),
        })
    }

    /// What `--verbose` would print.
    pub fn log(&self) -> &[String] {
        &self.log
    }

    async fn load_cursors(&self) -> Result<Vec<Cursor>, ImportError> {
        let database = self.database;
        let rows = sqlx::query(&format!(
            "SELECT id, refType, refNameRaw, refNameEncoding, isPermanent FROM {} WHERE \
             repositoryPHID = ? ORDER BY id DESC",
            database.table("repository", "repository_refcursor")
        ))
        .bind(&self.repository.phid)
        .fetch_all(database.pool())
        .await?;
        let mut cursors = Vec::new();
        for row in &rows {
            let raw: Vec<u8> =
                sqlx::Row::try_get(row, "refNameRaw").map_err(phab_db::DbError::from)?;
            let encoding = rows::opt_text(row, "refNameEncoding")?;
            let (Ok(name), Some("utf8")) = (String::from_utf8(raw), encoding.as_deref()) else {
                return Err(ImportError::unsupported("a ref name not stored as UTF-8"));
            };
            cursors.push(Cursor {
                id: rows::int(row, "id")?,
                ref_type: rows::text(row, "refType")?,
                name,
                is_permanent: rows::flag(row, "isPermanent")?,
                positions: Vec::new(),
            });
        }
        if !cursors.is_empty() {
            let ids: Vec<i64> = cursors.iter().map(|cursor| cursor.id).collect();
            let sql = format!(
                "SELECT id, cursorID, commitIdentifier FROM {} WHERE cursorID IN ({}) ORDER BY id",
                database.table("repository", "repository_refposition"),
                placeholders(ids.len())
            );
            for row in bind_all(sqlx::query(&sql), ids)
                .fetch_all(database.pool())
                .await?
            {
                let cursor_id = rows::int(&row, "cursorID")?;
                if let Some(cursor) = cursors.iter_mut().find(|cursor| cursor.id == cursor_id) {
                    cursor.positions.push((
                        rows::int(&row, "id")?,
                        rows::text(&row, "commitIdentifier")?,
                    ));
                }
            }
        }
        Ok(cursors)
    }

    /// `updateRefs()`.
    pub async fn update_refs(&mut self) -> Result<(), ImportError> {
        let found = refs::git_refs(&self.copy, &[BRANCH, TAG, REF]).await?;
        // mgroup() by type, in the order the types first show up, then the
        // missing types.
        let mut maps: IndexMap<&'static str, Vec<GitRef>> = IndexMap::new();
        for git_ref in found {
            maps.entry(git_ref.ref_type).or_default().push(git_ref);
        }
        for kind in [BRANCH, TAG, BOOKMARK, REF] {
            maps.entry(kind).or_default();
        }
        let all_cursors = self.load_cursors().await?;

        let mut closing_heads: IndexMap<String, ()> = IndexMap::new();
        for cursor in all_cursors.iter().filter(|cursor| cursor.is_permanent) {
            for (_, identifier) in &cursor.positions {
                closing_heads.insert(identifier.clone(), ());
            }
        }
        let closing_heads = self
            .remove_missing_commits(closing_heads.into_keys().collect())
            .await?;

        for (kind, refs) in &maps {
            let group: Vec<&Cursor> = all_cursors
                .iter()
                .filter(|cursor| cursor.ref_type == *kind)
                .collect();
            self.update_cursors(&group, refs, kind, &closing_heads)
                .await?;
        }

        if !self.permanent_commits.is_empty() {
            let identifiers: Vec<String> = self.permanent_commits.keys().cloned().collect();
            self.set_permanent_flag_on_commits(&identifiers).await?;
        }

        let mut save_cursors = Vec::new();
        for cursor in &all_cursors {
            let is_permanent = self
                .repository
                .is_permanent_ref(&cursor.ref_type, &cursor.name)?;
            if is_permanent != cursor.is_permanent {
                save_cursors.push((cursor.id, is_permanent));
            }
        }

        if !self.new_positions.is_empty()
            || !self.dead_positions.is_empty()
            || !save_cursors.is_empty()
        {
            let database = self.database;
            let mut transaction = database.pool().begin().await?;
            for (cursor_id, identifier) in std::mem::take(&mut self.new_positions) {
                sqlx::query(&format!(
                    "INSERT IGNORE INTO {} (cursorID, commitIdentifier, isClosed) VALUES (?, ?, 0)",
                    database.table("repository", "repository_refposition")
                ))
                .bind(cursor_id)
                .bind(identifier)
                .execute(&mut *transaction)
                .await?;
            }
            for (position_id, identifier) in std::mem::take(&mut self.dead_positions) {
                sqlx::query(&format!(
                    "INSERT INTO {} (repositoryPHID, commitIdentifier) VALUES (?, ?)",
                    database.table("repository", "repository_oldref")
                ))
                .bind(&self.repository.phid)
                .bind(identifier)
                .execute(&mut *transaction)
                .await?;
                sqlx::query(&format!(
                    "DELETE FROM {} WHERE id = ?",
                    database.table("repository", "repository_refposition")
                ))
                .bind(position_id)
                .execute(&mut *transaction)
                .await?;
            }
            for (cursor_id, is_permanent) in save_cursors {
                sqlx::query(&format!(
                    "UPDATE {} SET isPermanent = ? WHERE id = ?",
                    database.table("repository", "repository_refcursor")
                ))
                .bind(i64::from(is_permanent))
                .bind(cursor_id)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
        }
        Ok(())
    }

    /// `removeMissingCommits()`: the heads Git still has.
    async fn remove_missing_commits(
        &self,
        identifiers: Vec<String>,
    ) -> Result<Vec<String>, ImportError> {
        if identifiers.is_empty() {
            return Ok(identifiers);
        }
        let resolved = refs::resolve(&self.copy, &identifiers, None).await?;
        Ok(identifiers
            .into_iter()
            .filter(|identifier| {
                resolved
                    .get(&crate::php::Key::from_text(identifier))
                    .is_some_and(|matches| !matches.is_empty())
            })
            .collect())
    }

    async fn update_cursors(
        &mut self,
        cursors: &[&Cursor],
        new_refs: &[GitRef],
        ref_type: &str,
        closing_heads: &[String],
    ) -> Result<(), ImportError> {
        // mpull(..., 'getRefNameRaw'): the last cursor of a name wins.
        let mut by_name: IndexMap<&str, &Cursor> = IndexMap::new();
        for cursor in cursors {
            by_name.insert(cursor.name.as_str(), cursor);
        }
        let mut groups: IndexMap<&str, Vec<&GitRef>> = IndexMap::new();
        for git_ref in new_refs {
            groups
                .entry(git_ref.short_name.as_str())
                .or_default()
                .push(git_ref);
        }
        for (name, group) in &groups {
            let mut new_commits: IndexMap<String, ()> = IndexMap::new();
            for git_ref in group {
                new_commits.insert(git_ref.commit.clone(), ());
            }
            let cursor = by_name.get(name).copied();
            let old_positions: Vec<(i64, String)> = cursor
                .map(|cursor| cursor.positions.clone())
                .unwrap_or_default();
            for (position_id, hash) in &old_positions {
                if new_commits.contains_key(hash) {
                    self.log
                        .push(format!("Ref {ref_type} \"{name}\" still points at {hash}."));
                    continue;
                }
                self.log.push(format!(
                    "Ref {ref_type} \"{name}\" no longer points at {hash}."
                ));
                self.dead_positions.push((*position_id, hash.clone()));
            }
            let added: Vec<String> = new_commits
                .keys()
                .filter(|hash| !old_positions.iter().any(|(_, old)| old == *hash))
                .cloned()
                .collect();
            let mut cursor_state: Option<(i64, bool)> =
                cursor.map(|cursor| (cursor.id, cursor.is_permanent));
            for identifier in &added {
                self.log.push(format!(
                    "Ref {ref_type} \"{name}\" now points at {identifier}."
                ));
                if cursor_state.is_none() {
                    cursor_state = Some(self.new_ref_cursor(ref_type, name).await?);
                }
                if let Some((cursor_id, _)) = cursor_state {
                    self.new_positions.push((cursor_id, identifier.clone()));
                }
            }
            let head = group[0];
            if self
                .repository
                .is_permanent_ref(head.ref_type, &head.short_name)?
            {
                // A cursor which just became permanent publishes everything.
                let was_permanent = cursor_state.is_some_and(|(_, permanent)| permanent);
                let update: Vec<String> = if was_permanent {
                    added.clone()
                } else {
                    new_commits.keys().cloned().collect()
                };
                for identifier in update {
                    for commit in self
                        .load_new_commit_identifiers(&identifier, closing_heads)
                        .await?
                    {
                        self.permanent_commits.insert(commit, ());
                    }
                }
            }
        }
        // Refs which are gone: their positions die.
        for (name, cursor) in &by_name {
            if groups.contains_key(name) {
                continue;
            }
            for (position_id, hash) in &cursor.positions {
                self.log.push(format!(
                    "Ref {} \"{}\" no longer exists.",
                    cursor.ref_type, cursor.name
                ));
                self.dead_positions.push((*position_id, hash.clone()));
            }
        }
        Ok(())
    }

    /// `loadNewCommitIdentifiers()`: the ancestors of `head` which no old
    /// closing head has.
    async fn load_new_commit_identifiers(
        &self,
        head: &str,
        closing_heads: &[String],
    ) -> Result<Vec<String>, ImportError> {
        let output = if closing_heads.is_empty() {
            self.copy
                .exec_with_timeout(
                    vec![
                        "log".to_owned(),
                        "--format=%H".to_owned(),
                        ref_selector(head)?,
                        "--".to_owned(),
                    ],
                    None,
                    LOG_TIMEOUT,
                )
                .await?
        } else {
            let mut list = vec![head.to_owned()];
            list.extend(closing_heads.iter().map(|old| format!("^{old}")));
            list.push("--".to_owned());
            let input = format!("{}\n", list.join("\n"));
            self.copy
                .exec_with_timeout(
                    vec![
                        "log".to_owned(),
                        "--format=%H".to_owned(),
                        "--stdin".to_owned(),
                        "--".to_owned(),
                    ],
                    Some(input.into_bytes()),
                    LOG_TIMEOUT,
                )
                .await?
        };
        if !output.success() {
            return Err(ImportError::Failed(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let stdout = crate::php::text(output.stdout)?;
        let stdout = crate::php::trim(&stdout);
        if stdout.is_empty() {
            return Ok(Vec::new());
        }
        Ok(crate::import::text::split_lines(stdout)
            .into_iter()
            .map(str::to_owned)
            .collect())
    }

    /// `setPermanentFlagOnCommits()`.
    async fn set_permanent_flag_on_commits(
        &self,
        identifiers: &[String],
    ) -> Result<(), ImportError> {
        let database = self.database;
        let table = database.table("repository", "repository_commit");
        let mut found: std::collections::HashMap<String, (i64, String, i64)> =
            std::collections::HashMap::new();
        for chunk in identifiers.chunks(QUERY_CHUNK) {
            let sql = format!(
                "SELECT id, phid, commitIdentifier, importStatus FROM {table} WHERE \
                 repositoryID = ? AND commitIdentifier IN ({})",
                placeholders(chunk.len())
            );
            for row in bind_all(sqlx::query(&sql).bind(self.repository.id), chunk)
                .fetch_all(database.pool())
                .await?
            {
                found.insert(
                    rows::text(&row, "commitIdentifier")?,
                    (
                        rows::int(&row, "id")?,
                        rows::text(&row, "phid")?,
                        rows::int(&row, "importStatus")?,
                    ),
                );
            }
        }
        let priority = import_task_priority(self.repository, identifiers.len());
        for identifier in identifiers {
            let Some((id, phid, status)) = found.get(identifier) else {
                return Err(ImportError::Failed(format!(
                    "Commit \"{identifier}\" has not been discovered yet! Run discovery before \
                     updating refs."
                )));
            };
            if status & IMPORTED_PERMANENT == 0 {
                // Publishing runs again (T13580).
                let status = (status | IMPORTED_PERMANENT) & !IMPORTED_PUBLISH;
                sqlx::query(&format!("UPDATE {table} SET importStatus = ? WHERE id = ?"))
                    .bind(status)
                    .bind(id)
                    .execute(database.pool())
                    .await?;
                queue_commit_import_task(database, self.repository, phid, priority, "ref").await?;
            }
        }
        Ok(())
    }

    /// `newRefCursor()`: the cursor of a ref seen for the first time, or
    /// the one another process just created. Returns its ID and whether it
    /// is permanent.
    async fn new_ref_cursor(&self, ref_type: &str, name: &str) -> Result<(i64, bool), ImportError> {
        let database = self.database;
        let table = database.table("repository", "repository_refcursor");
        let is_permanent = self.repository.is_permanent_ref(ref_type, name)?;
        let inserted = sqlx::query(&format!(
            "INSERT INTO {table} (phid, repositoryPHID, refType, refNameHash, refNameRaw, \
             refNameEncoding, isPermanent) VALUES (?, ?, ?, ?, ?, 'utf8', ?)"
        ))
        .bind(Phid::generate(REF_CURSOR, None).into_string())
        .bind(&self.repository.phid)
        .bind(ref_type)
        .bind(digest_for_index(name.as_bytes()))
        .bind(name.as_bytes())
        .bind(i64::from(is_permanent))
        .execute(database.pool())
        .await;
        match inserted {
            Ok(done) => Ok((
                i64::try_from(done.last_insert_id()).unwrap_or_default(),
                is_permanent,
            )),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                let row = sqlx::query(&format!(
                    "SELECT id, isPermanent FROM {table} WHERE repositoryPHID = ? AND refType = ? \
                     AND refNameHash = ?"
                ))
                .bind(&self.repository.phid)
                .bind(ref_type)
                .bind(digest_for_index(name.as_bytes()))
                .fetch_optional(database.pool())
                .await?
                .ok_or_else(|| {
                    ImportError::Failed(format!(
                        "Failed to create a new ref cursor (for \"{name}\", of type \
                         \"{ref_type}\", in repository \"{}\") because it collided with an \
                         existing cursor, but then failed to load that cursor.",
                        self.repository.display_name()
                    ))
                })?;
                Ok((rows::int(&row, "id")?, rows::flag(&row, "isPermanent")?))
            }
            Err(error) => Err(error.into()),
        }
    }
}
