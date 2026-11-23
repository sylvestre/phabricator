//! `PhabricatorRepositoryCommit` and `PhabricatorRepositoryCommitData`, and
//! what `PhabricatorRepositoryCommitParserWorker` does before and after
//! each step: load the commit of the task, check its import flags, queue
//! the next step.

use phab_core::php;
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value, json};
use sqlx::mysql::MySqlRow;

use crate::import::repository::ImportRepository;
use crate::import::{IMPORTED_PERMANENT, IMPORTED_UNREACHABLE, ImportError, PRIORITY_DEFAULT};

/// A row of `repository_commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// `id`.
    pub id: i64,
    /// `repositoryID`.
    pub repository_id: i64,
    /// `phid`.
    pub phid: String,
    /// `authorIdentityPHID`.
    pub author_identity_phid: Option<String>,
    /// `committerIdentityPHID`.
    pub committer_identity_phid: Option<String>,
    /// `commitIdentifier`.
    pub identifier: String,
    /// `epoch`.
    pub epoch: i64,
    /// `authorPHID`.
    pub author_phid: Option<String>,
    /// `auditStatus`.
    pub audit_status: String,
    /// `summary`.
    pub summary: String,
    /// `importStatus`.
    pub import_status: i64,
}

const COLUMNS: &str = "id, repositoryID, phid, authorIdentityPHID, committerIdentityPHID, \
     commitIdentifier, epoch, authorPHID, auditStatus, summary, importStatus";

impl Commit {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            repository_id: rows::int(row, "repositoryID")?,
            phid: rows::text(row, "phid")?,
            author_identity_phid: rows::opt_text(row, "authorIdentityPHID")?,
            committer_identity_phid: rows::opt_text(row, "committerIdentityPHID")?,
            identifier: rows::text(row, "commitIdentifier")?,
            epoch: rows::int(row, "epoch")?,
            author_phid: rows::opt_text(row, "authorPHID")?,
            audit_status: rows::text(row, "auditStatus")?,
            summary: rows::text(row, "summary")?,
            import_status: rows::int(row, "importStatus")?,
        })
    }

    async fn load_where(
        database: &Database,
        column: &str,
        value: Value,
    ) -> Result<Option<Self>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM {} WHERE {column} = ?",
            database.table("repository", "repository_commit")
        );
        let query = sqlx::query(&sql);
        let query = match value {
            Value::Number(number) => query.bind(number.as_i64()),
            other => query.bind(php::string_cast(&other).into_owned()),
        };
        query
            .fetch_optional(database.pool())
            .await?
            .as_ref()
            .map(Self::from_row)
            .transpose()
    }

    /// The commit with PHID `phid`.
    pub async fn load_by_phid(database: &Database, phid: &str) -> Result<Option<Self>, DbError> {
        Self::load_where(database, "phid", json!(phid)).await
    }

    /// The commit with ID `id`.
    pub async fn load_by_id(database: &Database, id: i64) -> Result<Option<Self>, DbError> {
        Self::load_where(database, "id", json!(id)).await
    }

    /// The commit of `repository` with this identifier.
    pub async fn load_by_identifier(
        database: &Database,
        repository_id: i64,
        identifier: &str,
    ) -> Result<Option<Self>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM {} WHERE repositoryID = ? AND commitIdentifier = ?",
            database.table("repository", "repository_commit")
        );
        sqlx::query(&sql)
            .bind(repository_id)
            .bind(identifier)
            .fetch_optional(database.pool())
            .await?
            .as_ref()
            .map(Self::from_row)
            .transpose()
    }

    /// The parents of the commit (`repository_parents`), as rows.
    pub async fn load_parents(&self, database: &Database) -> Result<Vec<Self>, DbError> {
        let columns = COLUMNS
            .split(", ")
            .map(|column| format!("commit.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {columns} FROM {} commit JOIN {} parents ON commit.id = \
             parents.parentCommitID WHERE parents.childCommitID = ? ORDER BY \
             parents.parentCommitID",
            database.table("repository", "repository_commit"),
            database.table("repository", "repository_parents"),
        );
        sqlx::query(&sql)
            .bind(self.id)
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(Self::from_row)
            .collect()
    }

    /// `isPartiallyImported()`.
    pub fn is_partially_imported(&self, mask: i64) -> bool {
        self.import_status & mask == mask
    }

    /// `isUnreachable()`.
    pub fn is_unreachable(&self) -> bool {
        self.is_partially_imported(IMPORTED_UNREACHABLE)
    }

    /// `isPermanentCommit()`.
    pub fn is_permanent(&self) -> bool {
        self.is_partially_imported(IMPORTED_PERMANENT)
    }

    /// `getMonogram()`.
    pub fn monogram(&self, repository: &ImportRepository) -> String {
        match repository.callsign.as_deref() {
            Some(callsign) => format!("r{callsign}{}", self.identifier),
            None => format!("R{}:{}", repository.id, self.identifier),
        }
    }

    /// `writeImportStatusFlag()`.
    pub async fn write_import_status_flag(
        &mut self,
        database: &Database,
        flag: i64,
    ) -> Result<(), DbError> {
        sqlx::query(&format!(
            "UPDATE {} SET importStatus = (importStatus | ?) WHERE id = ?",
            database.table("repository", "repository_commit")
        ))
        .bind(flag)
        .bind(self.id)
        .execute(database.pool())
        .await?;
        self.import_status |= flag;
        Ok(())
    }

    /// `save()` of a loaded commit: every column but `importStatus`
    /// (`CONFIG_NO_MUTATE`).
    pub async fn save(&self, database: &Database) -> Result<(), DbError> {
        sqlx::query(&format!(
            "UPDATE {} SET repositoryID = ?, phid = ?, authorIdentityPHID = ?, \
             committerIdentityPHID = ?, commitIdentifier = ?, epoch = ?, authorPHID = ?, \
             auditStatus = ?, summary = ? WHERE id = ?",
            database.table("repository", "repository_commit")
        ))
        .bind(self.repository_id)
        .bind(&self.phid)
        .bind(&self.author_identity_phid)
        .bind(&self.committer_identity_phid)
        .bind(&self.identifier)
        .bind(self.epoch)
        .bind(&self.author_phid)
        .bind(&self.audit_status)
        .bind(&self.summary)
        .bind(self.id)
        .execute(database.pool())
        .await?;
        Ok(())
    }
}

/// A row of `repository_commitdata`.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitData {
    /// `id`; `None` until saved.
    pub id: Option<i64>,
    /// `commitID`.
    pub commit_id: i64,
    /// `authorName`.
    pub author_name: String,
    /// `commitMessage`.
    pub commit_message: String,
    /// `commitDetails`, decoded.
    pub details: Map<String, Value>,
}

impl CommitData {
    /// `loadCommitData()`: the row of the commit, or a new one.
    pub async fn load(database: &Database, commit_id: i64) -> Result<Self, ImportError> {
        Ok(Self::load_existing(database, commit_id)
            .await?
            .unwrap_or_else(|| Self {
                id: None,
                commit_id,
                author_name: String::new(),
                commit_message: String::new(),
                details: Map::new(),
            }))
    }

    /// The row of the commit, if there is one.
    pub async fn load_existing(
        database: &Database,
        commit_id: i64,
    ) -> Result<Option<Self>, ImportError> {
        let row = sqlx::query(&format!(
            "SELECT id, commitID, authorName, commitMessage, commitDetails FROM {} \
             WHERE commitID = ?",
            database.table("repository", "repository_commitdata")
        ))
        .bind(commit_id)
        .fetch_optional(database.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let details = match serde_json::from_str::<Value>(&rows::text(&row, "commitDetails")?) {
            Ok(Value::Object(details)) => details,
            Ok(Value::Array(list)) if list.is_empty() => Map::new(),
            Ok(Value::Null) => Map::new(),
            _ => {
                return Err(ImportError::unsupported(
                    "commit details which are not a dictionary",
                ));
            }
        };
        Ok(Some(Self {
            id: Some(rows::int(&row, "id")?),
            commit_id: rows::int(&row, "commitID")?,
            author_name: rows::text(&row, "authorName")?,
            commit_message: rows::text(&row, "commitMessage")?,
            details,
        }))
    }

    /// `getCommitDetail()`.
    pub fn detail(&self, key: &str) -> Option<&Value> {
        self.details.get(key)
    }

    /// `setCommitDetail()`: a key set again keeps its place.
    pub fn set_detail(&mut self, key: &str, value: Value) {
        self.details.insert(key.to_owned(), value);
    }

    /// `save()`: an insert for a new row, else an update of every column.
    pub async fn save(&mut self, database: &Database) -> Result<(), DbError> {
        let table = database.table("repository", "repository_commitdata");
        let details = php::json_encode(&php::round_trip(&Value::Object(self.details.clone())));
        if let Some(id) = self.id {
            sqlx::query(&format!(
                "UPDATE {table} SET commitID = ?, authorName = ?, commitMessage = ?, \
                 commitDetails = ? WHERE id = ?"
            ))
            .bind(self.commit_id)
            .bind(&self.author_name)
            .bind(&self.commit_message)
            .bind(details)
            .bind(id)
            .execute(database.pool())
            .await?;
        } else {
            let id = sqlx::query(&format!(
                "INSERT INTO {table} (commitID, authorName, commitMessage, commitDetails) \
                 VALUES (?, ?, ?, ?)"
            ))
            .bind(self.commit_id)
            .bind(&self.author_name)
            .bind(&self.commit_message)
            .bind(details)
            .execute(database.pool())
            .await?
            .last_insert_id();
            self.id = i64::try_from(id).ok();
        }
        Ok(())
    }
}

/// What a parser task works on: its data, its commit and repository.
pub struct ParserTask {
    /// The task data, a dictionary.
    pub data: Map<String, Value>,
    /// The commit.
    pub commit: Commit,
    /// Its repository.
    pub repository: ImportRepository,
}

/// A follow-up task, as `queueCommitTask()` queues it.
#[derive(Debug, Clone, PartialEq)]
pub struct FollowUp {
    /// The task class.
    pub class: &'static str,
    /// The task data.
    pub data: Value,
    /// The priority.
    pub priority: i64,
    /// `objectPHID`: the commit.
    pub object_phid: String,
    /// `containerPHID`: the repository.
    pub container_phid: String,
}

impl ParserTask {
    /// `loadCommit()`: the commit of `commitPHID` (or of the legacy
    /// `commitID`), which must exist and be reachable, and its repository,
    /// which the pipeline must support. Fails permanently as PHP does.
    pub async fn load(database: &Database, data: &Value) -> Result<Self, ImportError> {
        let data = match data {
            Value::Object(data) => data.clone(),
            Value::Array(list) if list.is_empty() => Map::new(),
            _ => {
                return Err(ImportError::unsupported(
                    "task data which is not a dictionary",
                ));
            }
        };
        let mut phid = match data.get("commitPHID") {
            Some(Value::String(phid)) if php::is_truthy_str(phid) => Some(phid.clone()),
            Some(value) if php::is_truthy(value) => {
                return Err(ImportError::unsupported(
                    "a commit PHID which is not a string",
                ));
            }
            _ => None,
        };
        if phid.is_none()
            && let Some(id) = data.get("commitID").filter(|id| php::is_truthy(id))
        {
            let id = match id {
                Value::Number(number) => number.as_i64(),
                Value::String(text) => text.parse().ok(),
                _ => None,
            }
            .ok_or_else(|| ImportError::unsupported("a commit ID which is not an integer"))?;
            if let Some(commit) = Commit::load_by_id(database, id).await? {
                phid = Some(commit.phid);
            }
        }
        let Some(phid) = phid else {
            return Err(ImportError::Permanent(
                "Task data has no \"commitPHID\".".to_owned(),
            ));
        };
        let commit = Commit::load_by_phid(database, &phid).await?;
        let repository = match &commit {
            Some(commit) => ImportRepository::load(database, commit.repository_id).await?,
            None => None,
        };
        // DiffusionCommitQuery drops commits whose repository doesn't load.
        let (Some(commit), Some(repository)) = (commit, repository) else {
            return Err(ImportError::Permanent(format!(
                "Commit \"{phid}\" does not exist."
            )));
        };
        repository.check_supported()?;
        if commit.is_unreachable() {
            return Err(ImportError::Permanent(format!(
                "Commit \"{}\" (with PHID \"{phid}\") is no longer reachable from any branch, \
                 tag, or ref in this repository, so it will not be imported. This usually \
                 means that the branch the commit was on was deleted or overwritten.",
                commit.monogram(&repository)
            )));
        }
        Ok(Self {
            data,
            commit,
            repository,
        })
    }

    /// `shouldQueueFollowupTasks()`: tasks queued by `bin/repository
    /// reparse` (`only`) don't.
    pub fn should_queue_followups(&self) -> bool {
        !self.data.get("only").is_some_and(php::is_truthy)
    }

    /// `shouldSkipImportStep()`: a step already done is not done again by
    /// a task the daemons queued.
    pub fn should_skip_step(&self, flag: i64) -> bool {
        self.commit.is_partially_imported(flag) && self.should_queue_followups()
    }

    /// `queueCommitTask()`.
    pub fn followup(&self, class: &'static str) -> Option<FollowUp> {
        if !self.should_queue_followups() {
            return None;
        }
        let mut data = Map::new();
        data.insert("commitPHID".to_owned(), json!(self.commit.phid));
        // isset(): a null "via" is not copied.
        if let Some(via) = self.data.get("via").filter(|via| !via.is_null()) {
            data.insert("via".to_owned(), via.clone());
        }
        Some(FollowUp {
            class,
            data: Value::Object(data),
            priority: PRIORITY_DEFAULT,
            object_phid: self.commit.phid.clone(),
            container_phid: self.repository.phid.clone(),
        })
    }
}
