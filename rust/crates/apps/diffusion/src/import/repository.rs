//! `PhabricatorRepository`, as the import pipeline reads and writes it:
//! its details, the branch filters, the publisher
//! (`PhabricatorRepositoryPublisher`) and the status messages.

use phab_core::php;
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value};

use crate::git::WorkingCopy;
use crate::import::{ImportError, now};
use crate::refs::{BRANCH, GitRef, TAG};

/// `PhabricatorRepositoryPublisher::HOLD_IMPORTING`.
pub const HOLD_IMPORTING: &str = "auto/importing";
/// `PhabricatorRepositoryPublisher::HOLD_PUBLISHING_DISABLED`.
pub const HOLD_PUBLISHING_DISABLED: &str = "auto/disabled";
/// `PhabricatorRepositoryPublisher::HOLD_NOT_REACHABLE_FROM_PERMANENT_REF`.
pub const HOLD_NOT_REACHABLE_FROM_PERMANENT_REF: &str = "auto/nobranch";

/// `PhabricatorRepositoryStatusMessage::TYPE_INIT`.
pub const STATUS_INIT: &str = "init";
/// `PhabricatorRepositoryStatusMessage::TYPE_FETCH`.
pub const STATUS_FETCH: &str = "fetch";
/// `PhabricatorRepositoryStatusMessage::TYPE_NEEDS_UPDATE`.
pub const STATUS_NEEDS_UPDATE: &str = "needs-update";
/// `PhabricatorRepositoryStatusMessage::CODE_OKAY`.
pub const CODE_OKAY: &str = "okay";
/// `PhabricatorRepositoryStatusMessage::CODE_ERROR`.
pub const CODE_ERROR: &str = "error";

/// A row of `repository`.
#[derive(Debug, Clone)]
pub struct ImportRepository {
    /// `id`.
    pub id: i64,
    /// `phid`.
    pub phid: String,
    /// `name`.
    pub name: String,
    /// `callsign`.
    pub callsign: Option<String>,
    /// `versionControlSystem`.
    pub vcs: String,
    /// `details`, decoded.
    pub details: Map<String, Value>,
    /// `localPath`.
    pub local_path: Option<String>,
    /// `almanacServicePHID`.
    pub almanac_service_phid: Option<String>,
    /// `credentialPHID`.
    pub credential_phid: Option<String>,
    /// `repositorySlug`.
    pub slug: Option<String>,
}

/// A branch filter (`branch-filter`, `close-commits-filter`): the names it
/// lets through, or `None` for no filter.
fn filter_names(
    details: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, ImportError> {
    match details.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(filter)) if filter.is_empty() => Ok(None),
        Some(Value::Array(filter)) if filter.is_empty() => Ok(None),
        Some(Value::Object(filter)) => {
            let names: Vec<String> = filter.keys().cloned().collect();
            if names.iter().any(|name| name.starts_with("regexp(")) {
                return Err(ImportError::unsupported(
                    "branch filters with regular expressions",
                ));
            }
            Ok(Some(names))
        }
        Some(value) if !php::is_truthy(value) => Ok(None),
        Some(_) => Err(ImportError::unsupported(
            "a branch filter which is not a map",
        )),
    }
}

impl ImportRepository {
    /// The repository with ID `id`.
    pub async fn load(database: &Database, id: i64) -> Result<Option<Self>, DbError> {
        let row = sqlx::query(&format!(
            "SELECT id, phid, name, callsign, versionControlSystem, details, localPath, \
             almanacServicePHID, credentialPHID, repositorySlug FROM {} WHERE id = ?",
            database.table("repository", "repository")
        ))
        .bind(id)
        .fetch_optional(database.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let details = match rows::json(&row, "details") {
            Ok(Value::Object(details)) => details,
            _ => Map::new(),
        };
        Ok(Some(Self {
            id: rows::int(&row, "id")?,
            phid: rows::text(&row, "phid")?,
            name: rows::text(&row, "name")?,
            callsign: rows::opt_text(&row, "callsign")?,
            vcs: rows::text(&row, "versionControlSystem")?,
            details,
            local_path: rows::opt_text(&row, "localPath")?,
            almanac_service_phid: rows::opt_text(&row, "almanacServicePHID")?,
            credential_phid: rows::opt_text(&row, "credentialPHID")?,
            slug: rows::opt_text(&row, "repositorySlug")?,
        }))
    }

    /// The repository named by `identifier` (an ID, a monogram, a PHID, a
    /// callsign or a short name), as `bin/repository` finds it.
    pub async fn find(database: &Database, identifier: &str) -> Result<Option<Self>, DbError> {
        let found = phab_app_differential::repository::load_by_identifiers(
            database,
            &[identifier.to_owned()],
        )
        .await?;
        match found.get(identifier) {
            Some(repository) => Self::load(database, repository.id).await,
            None => Ok(None),
        }
    }

    /// The repository with PHID `phid`.
    pub async fn load_by_phid(database: &Database, phid: &str) -> Result<Option<Self>, DbError> {
        let id = sqlx::query_scalar::<_, u32>(&format!(
            "SELECT id FROM {} WHERE phid = ?",
            database.table("repository", "repository")
        ))
        .bind(phid)
        .fetch_optional(database.pool())
        .await?;
        match id {
            Some(id) => Self::load(database, i64::from(id)).await,
            None => Ok(None),
        }
    }

    /// The repositories the pipeline can work on: Git, not served by a
    /// cluster (whose working copies PHP synchronizes first), with a
    /// working copy on this host.
    pub fn check_supported(&self) -> Result<(), ImportError> {
        if self.vcs != "git" {
            return Err(ImportError::unsupported(format!(
                "a repository of type \"{}\"",
                self.vcs
            )));
        }
        if self
            .almanac_service_phid
            .as_deref()
            .is_some_and(php::is_truthy_str)
        {
            return Err(ImportError::unsupported("a repository served by a cluster"));
        }
        self.working_copy().map(|_| ())
    }

    /// `getDetail()`.
    pub fn detail(&self, key: &str) -> Option<&Value> {
        self.details.get(key)
    }

    /// The working copy (`getLocalPath()`), which must exist.
    pub fn working_copy(&self) -> Result<WorkingCopy, ImportError> {
        match self.local_path.as_deref() {
            Some(path) if std::path::Path::new(path).is_dir() => Ok(WorkingCopy::new(path)),
            _ => Err(ImportError::unsupported(
                "a working copy this host doesn't have",
            )),
        }
    }

    /// `getMonogram()`.
    pub fn monogram(&self) -> String {
        match self.callsign.as_deref() {
            Some(callsign) if !callsign.is_empty() => format!("r{callsign}"),
            _ => format!("R{}", self.id),
        }
    }

    /// `getDisplayName()`.
    pub fn display_name(&self) -> String {
        match self.slug.as_deref() {
            Some(slug) if !slug.is_empty() => slug.to_owned(),
            _ => self.monogram(),
        }
    }

    /// `isHosted()`.
    pub fn is_hosted(&self) -> bool {
        self.detail("hosting-enabled").is_some_and(php::is_truthy)
    }

    /// `isImporting()`.
    pub fn is_importing(&self) -> bool {
        self.detail("importing").is_some_and(php::is_truthy)
    }

    /// `isReadOnly()`.
    pub fn is_read_only(&self) -> bool {
        self.detail("read-only").is_some_and(php::is_truthy)
    }

    /// `getDefaultBranch()`.
    pub fn default_branch(&self) -> String {
        match self.detail("default-branch") {
            Some(Value::String(branch)) if !branch.is_empty() => branch.clone(),
            _ => "master".to_owned(),
        }
    }

    /// Checks the filters can be followed: neither may have a regular
    /// expression.
    pub fn check_filters(&self) -> Result<(), ImportError> {
        filter_names(&self.details, "branch-filter")?;
        filter_names(&self.details, "close-commits-filter")?;
        Ok(())
    }

    fn is_branch_in_filter(&self, branch: &str, key: &str) -> Result<bool, ImportError> {
        Ok(filter_names(&self.details, key)?
            .is_none_or(|names| names.iter().any(|name| name == branch)))
    }

    /// `shouldTrackBranch()`.
    pub fn should_track_branch(&self, branch: &str) -> Result<bool, ImportError> {
        self.is_branch_in_filter(branch, "branch-filter")
    }

    /// `shouldTrackRef()`.
    pub fn should_track_ref(&self, git_ref: &GitRef) -> Result<bool, ImportError> {
        if git_ref.ref_type == TAG && git_ref.short_name.starts_with("phabricator/") {
            return Ok(false);
        }
        if git_ref.ref_type != BRANCH {
            return Ok(true);
        }
        self.should_track_branch(&git_ref.short_name)
    }

    /// `PhabricatorRepositoryPublisher::isPermanentRef()` of a ref of this
    /// type and name.
    pub fn is_permanent_ref(&self, ref_type: &str, name: &str) -> Result<bool, ImportError> {
        Ok(ref_type == BRANCH
            && self.should_track_branch(name)?
            && self.is_branch_in_filter(name, "close-commits-filter")?)
    }

    /// `PhabricatorRepositoryPublisher::getRepositoryHoldReasons()`.
    pub fn hold_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.is_importing() {
            reasons.push(HOLD_IMPORTING);
        }
        if self.detail("herald-disabled").is_some_and(php::is_truthy) {
            reasons.push(HOLD_PUBLISHING_DISABLED);
        }
        reasons
    }

    /// `save()` after a change of the details: Lisk writes every column,
    /// and only these two differ from the row it loaded.
    pub async fn save_details(&self, database: &Database) -> Result<(), DbError> {
        let details = php::json_encode(&php::round_trip(&Value::Object(self.details.clone())));
        sqlx::query(&format!(
            "UPDATE {} SET details = ?, dateModified = ? WHERE id = ?",
            database.table("repository", "repository")
        ))
        .bind(details)
        .bind(now())
        .bind(self.id)
        .execute(database.pool())
        .await?;
        Ok(())
    }

    /// `markImporting()`.
    pub async fn mark_importing(&mut self, database: &Database) -> Result<(), DbError> {
        self.details
            .insert("importing".to_owned(), Value::Bool(true));
        self.save_details(database).await
    }

    /// `writeStatusMessage()`: deletes the message of `status_type` without
    /// a code, else writes it, counting the messages in a row with the
    /// same code.
    pub async fn write_status_message(
        &self,
        database: &Database,
        status_type: &str,
        code: Option<&str>,
        parameters: &Value,
    ) -> Result<(), DbError> {
        write_status_message(database, self.id, (status_type, code), parameters, now()).await
    }
}

/// `PhabricatorRepository::writeStatusMessage()` of the repository with
/// this ID, at `epoch`.
pub async fn write_status_message(
    database: &Database,
    repository_id: i64,
    (status_type, code): (&str, Option<&str>),
    parameters: &Value,
    epoch: i64,
) -> Result<(), DbError> {
    let table = database.table("repository", "repository_statusmessage");
    let Some(code) = code else {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE repositoryID = ? AND statusType = ?"
        ))
        .bind(repository_id)
        .bind(status_type)
        .execute(database.pool())
        .await?;
        return Ok(());
    };
    sqlx::query(&format!(
        "INSERT INTO {table} (repositoryID, statusType, statusCode, parameters, epoch, \
         messageCount) VALUES (?, ?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE messageCount = \
         IF(statusCode = VALUES(statusCode), messageCount + VALUES(messageCount), \
         VALUES(messageCount)), statusCode = VALUES(statusCode), parameters = \
         VALUES(parameters), epoch = VALUES(epoch)"
    ))
    .bind(repository_id)
    .bind(status_type)
    .bind(code)
    .bind(php::json_encode(parameters))
    .bind(epoch)
    .bind(1)
    .execute(database.pool())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn repository(details: Value) -> ImportRepository {
        let Value::Object(details) = details else {
            panic!("details must be a map");
        };
        ImportRepository {
            id: 3,
            phid: "PHID-REPO-x".to_owned(),
            name: "Fabrique".to_owned(),
            callsign: Some("FAB".to_owned()),
            vcs: "git".to_owned(),
            details,
            local_path: None,
            almanac_service_phid: None,
            credential_phid: None,
            slug: None,
        }
    }

    #[test]
    fn filters_branches_like_php() {
        let open = repository(json!({}));
        assert!(open.should_track_branch("x").unwrap());
        assert!(open.is_permanent_ref(BRANCH, "x").unwrap());
        assert!(!open.is_permanent_ref(TAG, "x").unwrap());
        let filtered = repository(json!({
            "branch-filter": {"main": true, "dev": true},
            "close-commits-filter": {"main": true},
        }));
        assert!(filtered.should_track_branch("dev").unwrap());
        assert!(!filtered.should_track_branch("x").unwrap());
        assert!(filtered.is_permanent_ref(BRANCH, "main").unwrap());
        assert!(!filtered.is_permanent_ref(BRANCH, "dev").unwrap());
        let empty = repository(json!({"branch-filter": []}));
        assert!(empty.should_track_branch("x").unwrap());
        let regexp = repository(json!({"branch-filter": {"regexp(/x/)": true}}));
        assert!(regexp.check_filters().is_err());
    }

    #[test]
    fn names_and_holds_like_php() {
        let fab = repository(json!({"importing": true, "herald-disabled": 1}));
        assert_eq!(fab.monogram(), "rFAB");
        assert_eq!(fab.display_name(), "rFAB");
        assert_eq!(
            fab.hold_reasons(),
            [HOLD_IMPORTING, HOLD_PUBLISHING_DISABLED]
        );
        assert_eq!(fab.default_branch(), "master");
    }
}
