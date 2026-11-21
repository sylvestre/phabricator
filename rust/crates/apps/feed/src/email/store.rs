//! What the email events read, as the omnipotent viewer: users and their
//! settings (`PhabricatorUserStore`), projects, revisions, transactions,
//! comments, diffs (`PhabricatorDiffStore`) and repositories. Lookups are
//! cached for the request, like the PHP stores.

use std::collections::HashMap;

use phab_auth::User;
use phab_conduit::ApiRequest;
use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, edges, rows};
use serde_json::{Map, Value};

use super::{Fail, Result};

/// `PhabricatorProjectMaterializedMemberEdgeType`.
const MATERIALIZED_MEMBER: i64 = edges::PROJECT_MATERIALIZED_MEMBER;
/// `PhabricatorUserPreferences::BUILTIN_GLOBAL_DEFAULT`.
const GLOBAL_PREFERENCES: &str = "global";

/// A column which PHP reads as a string: invalid UTF-8 can't be encoded to
/// JSON the way PHP fails to, so it is left to PHP.
pub(crate) fn strict_text(row: &MySqlRow, column: &str) -> Result<Option<String>> {
    use phab_db::sqlx::Row;
    let bytes: Option<Vec<u8>> = match row.try_get::<Option<Vec<u8>>, _>(column) {
        Ok(bytes) => bytes,
        Err(_) => row
            .try_get::<Option<String>, _>(column)
            .map(|text| text.map(String::into_bytes))
            .map_err(|_| Fail::Internal(format!("unreadable column {column}")))?,
    };
    bytes
        .map(|bytes| String::from_utf8(bytes).map_err(|_| Fail::Unsupported("invalid UTF-8 text")))
        .transpose()
}

/// A non-null [`strict_text()`] column.
pub(crate) fn required_text(row: &MySqlRow, column: &str) -> Result<String> {
    strict_text(row, column)?.ok_or_else(|| Fail::Internal(format!("null column {column}")))
}

/// The members of a project (`needMembers()`): its materialized members.
pub(crate) async fn project_members(
    database: &Database,
    project: &Phid,
) -> Result<Vec<Phid>, DbError> {
    edges::load_destination_phids(
        database,
        std::slice::from_ref(project),
        &[MATERIALIZED_MEMBER],
        None,
    )
    .await
}

/// `PhabricatorProjectQuery` with `needMembers()` and `needWatchers()`,
/// as the omnipotent viewer: `None` if the project doesn't exist or its
/// ancestors can't be loaded. Milestones take their name and members from
/// their parent.
pub(crate) async fn load_project(
    database: &Database,
    config: &Config,
    now: i64,
    phid: &Phid,
) -> Result<Option<ProjectInfo>> {
    let projects =
        phab_app_project::storage::load_by_phid(database, std::slice::from_ref(phid)).await?;
    let (visible, graph) = phab_app_project::policy::visible_projects(
        database,
        config,
        &phab_policy::Viewer::omnipotent(),
        projects,
        now,
    )
    .await
    .map_err(|error| Fail::Internal(error.to_string()))?;
    let Some(project) = visible.into_iter().next() else {
        return Ok(None);
    };
    let parent = graph.parent(&project);
    let source = match parent {
        Some(parent) if project.is_milestone() => parent.phid.clone(),
        _ => project.phid.clone(),
    };
    let members = project_members(database, &source).await?;
    let watchers = phab_app_project::storage::load_edge_destinations(
        database,
        &project.phid,
        phab_app_project::storage::OBJECT_HAS_WATCHER,
    )
    .await?;
    Ok(Some(ProjectInfo {
        name: project.display_name(parent),
        members,
        watchers,
    }))
}

/// A project as `PhabricatorUserStore::fetchProject()` loads it.
#[derive(Debug, Clone)]
pub(crate) struct ProjectInfo {
    /// `getDisplayName()`.
    pub(crate) name: String,
    /// `getMemberPHIDs()`.
    pub(crate) members: Vec<Phid>,
    /// `getWatcherPHIDs()`.
    pub(crate) watchers: Vec<Phid>,
}

/// A `differential_reviewer` row.
#[derive(Debug, Clone)]
pub(crate) struct ReviewerRow {
    pub(crate) phid: Phid,
    pub(crate) status: String,
    pub(crate) voided: bool,
}

/// A revision as `DifferentialRevisionQuery` loads it with
/// `needReviewers()` and `needActiveDiffs()`.
#[derive(Debug, Clone)]
pub(crate) struct RevisionRow {
    pub(crate) id: i64,
    pub(crate) phid: Phid,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) author_phid: Option<Phid>,
    pub(crate) repository_phid: Option<Phid>,
    pub(crate) active_diff_phid: Option<Phid>,
    /// The ID of the active diff, if it exists.
    pub(crate) active_diff_id: Option<i64>,
    pub(crate) properties: Value,
    /// In ID order.
    pub(crate) reviewers: Vec<ReviewerRow>,
}

impl RevisionRow {
    /// `getMonogram()`.
    pub(crate) fn monogram(&self) -> String {
        format!("D{}", self.id)
    }

    /// `getShouldBroadcast()`: the `draft.broadcast` property, true when
    /// it is not set.
    pub(crate) fn should_broadcast(&self) -> bool {
        match self.properties.get("draft.broadcast") {
            None => true,
            Some(value) => phab_core::php::is_truthy(value),
        }
    }

    /// The reviewers who haven't resigned.
    pub(crate) fn unresigned_reviewers(&self) -> impl Iterator<Item = &ReviewerRow> {
        self.reviewers
            .iter()
            .filter(|reviewer| reviewer.status != "resigned")
    }
}

/// A `differential_transaction` row.
#[derive(Debug, Clone)]
pub(crate) struct TransactionRow {
    pub(crate) id: i64,
    pub(crate) phid: Phid,
    pub(crate) author_phid: Phid,
    pub(crate) object_phid: Phid,
    pub(crate) comment_phid: Option<Phid>,
    pub(crate) transaction_type: String,
    pub(crate) old_value: Value,
    pub(crate) new_value: Value,
    pub(crate) metadata: Value,
}

impl TransactionRow {
    /// `getMetadataValue()`.
    pub(crate) fn metadata_value(&self, key: &str) -> Option<&Value> {
        self.metadata.get(key)
    }
}

/// A `differential_transaction_comment` row.
#[derive(Debug, Clone)]
pub(crate) struct CommentRow {
    pub(crate) id: i64,
    pub(crate) author_phid: Phid,
    pub(crate) content: String,
    pub(crate) changeset_id: Option<i64>,
    pub(crate) is_new_file: bool,
    pub(crate) line_number: i64,
    pub(crate) line_length: i64,
    pub(crate) reply_to_phid: Option<Phid>,
    pub(crate) attributes: Value,
    pub(crate) date_created: i64,
}

/// A changeset of [`DiffRow`], with the stored data of its hunks, which
/// `ResolveCodeChange` compares (`getData()`).
#[derive(Debug, Clone)]
pub(crate) struct RawChangeset {
    pub(crate) filename: String,
    pub(crate) file_type: i64,
    pub(crate) change_type: i64,
    pub(crate) metadata: Value,
    pub(crate) new_properties: Value,
    /// In ID order.
    pub(crate) hunk_data: Vec<Vec<u8>>,
}

impl RawChangeset {
    /// `getDisplayFilename()`.
    pub(crate) fn display_filename(&self) -> String {
        if self.file_type == 4 {
            format!("{}/", self.filename)
        } else {
            self.filename.clone()
        }
    }
}

/// A diff with its changesets, newest first (`DifferentialDiffQuery` with
/// `needChangesets()`).
#[derive(Debug, Clone)]
pub(crate) struct DiffRow {
    pub(crate) id: i64,
    pub(crate) changesets: Vec<RawChangeset>,
}

/// A `repository` row.
#[derive(Debug, Clone)]
pub(crate) struct RepositoryRow {
    pub(crate) phid: Phid,
    pub(crate) name: String,
    pub(crate) vcs: String,
}

/// A `repository_commit` row.
#[derive(Debug, Clone)]
pub(crate) struct CommitRow {
    pub(crate) identifier: String,
    pub(crate) repository_id: i64,
}

/// A value which was looked up, and may not exist.
#[derive(Debug, Clone)]
struct Loaded<T>(Option<T>);

/// The per-request lookups and their caches.
pub(crate) struct Store<'a> {
    pub(crate) database: &'a Database,
    pub(crate) config: &'a Config,
    /// The request time, for time zone offsets.
    pub(crate) now: i64,
    users: HashMap<Phid, Option<User>>,
    usernames: HashMap<String, Option<User>>,
    emails: HashMap<Phid, Option<String>>,
    preferences: HashMap<Phid, Map<String, Value>>,
    /// `None` until loaded; the global preferences may not exist.
    global_preferences: Option<Loaded<Map<String, Value>>>,
    projects: HashMap<Phid, Option<ProjectInfo>>,
    secure_project: Option<Phid>,
    diffs: HashMap<Phid, Option<DiffRow>>,
    pub(crate) bug_names: HashMap<String, String>,
    pub(crate) http: Option<reqwest::Client>,
}

fn phid_strings(phids: &[Phid]) -> impl Iterator<Item = &str> {
    phids.iter().map(Phid::as_str)
}

impl<'a> Store<'a> {
    pub(crate) fn new(request: &'a ApiRequest<'a>) -> Self {
        Self::with(
            &request.context.database,
            &request.context.config,
            request.now,
        )
    }

    pub(crate) fn with(database: &'a Database, config: &'a Config, now: i64) -> Self {
        Self {
            database,
            config,
            now,
            users: HashMap::new(),
            usernames: HashMap::new(),
            emails: HashMap::new(),
            preferences: HashMap::new(),
            global_preferences: None,
            projects: HashMap::new(),
            secure_project: None,
            diffs: HashMap::new(),
            bug_names: HashMap::new(),
            http: None,
        }
    }

    /// `PhabricatorUserStore::queryAll()`: the users, newest first like
    /// `PhabricatorPeopleQuery`.
    pub(crate) async fn users(&mut self, phids: &[Phid]) -> Result<Vec<User>> {
        if phids.is_empty() {
            return Ok(Vec::new());
        }
        let mut wanted = phids.to_vec();
        wanted.sort();
        wanted.dedup();
        let mut users = phab_auth::user::load_users_by_phid(self.database, &wanted).await?;
        users.sort_by_key(|user| std::cmp::Reverse(user.id));
        for user in &users {
            self.users.insert(user.phid.clone(), Some(user.clone()));
        }
        Ok(users)
    }

    /// `PhabricatorUserStore::find()`: its return type throws for a PHID
    /// which is not a user.
    pub(crate) async fn user(&mut self, phid: &Phid) -> Result<User> {
        if !self.users.contains_key(phid) {
            let found =
                phab_auth::user::load_users_by_phid(self.database, std::slice::from_ref(phid))
                    .await?
                    .into_iter()
                    .next();
            self.users.insert(phid.clone(), found);
        }
        self.users[phid].clone().ok_or(Fail::Throw(
            "PhabricatorUserStore::find() of an unknown user",
        ))
    }

    /// `PhabricatorUserStore::findByName()`: an enabled user, compared with
    /// the collation of `userName`.
    pub(crate) async fn user_by_name(&mut self, name: &str) -> Result<Option<User>> {
        if let Some(user) = self.usernames.get(name) {
            return Ok(user.clone());
        }
        let sql = format!(
            "SELECT {} FROM {} u WHERE u.userName = ? AND u.isDisabled = 0",
            phab_auth::user::select_columns("u"),
            self.database.table("user", "user"),
        );
        let found = phab_db::sqlx::query(&sql)
            .bind(name)
            .fetch_all(self.database.pool())
            .await?;
        if found.len() > 1 {
            return Err(Fail::Throw("several users with one name"));
        }
        let user = found.first().map(User::from_row).transpose()?;
        self.usernames.insert(name.to_owned(), user.clone());
        Ok(user)
    }

    /// `PhabricatorUser::loadPrimaryEmailAddress()`; `loadOneWhere()`
    /// throws for several primary addresses.
    pub(crate) async fn primary_email(&mut self, user: &Phid) -> Result<Option<String>> {
        if let Some(email) = self.emails.get(user) {
            return Ok(email.clone());
        }
        let sql = format!(
            "SELECT address FROM {} WHERE userPHID = ? AND isPrimary = 1",
            self.database.table("user", "user_email")
        );
        let found = phab_db::sqlx::query(&sql)
            .bind(user.as_str())
            .fetch_all(self.database.pool())
            .await?;
        if found.len() > 1 {
            return Err(Fail::Throw("several primary email addresses"));
        }
        let email = found
            .first()
            .map(|row| required_text(row, "address"))
            .transpose()?;
        self.emails.insert(user.clone(), email.clone());
        Ok(email)
    }

    /// The stored preferences of a user, empty without a row
    /// (`needSyntheticPreferences()`).
    pub(crate) async fn preferences(&mut self, user: &Phid) -> Result<Map<String, Value>> {
        if let Some(preferences) = self.preferences.get(user) {
            return Ok(preferences.clone());
        }
        let sql = format!(
            "SELECT preferences FROM {} WHERE userPHID = ?",
            self.database.table("user", "user_preferences")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(user.as_str())
            .fetch_optional(self.database.pool())
            .await?;
        let preferences = match row {
            Some(row) => preference_map(rows::json(&row, "preferences")?)?,
            None => Map::new(),
        };
        self.preferences.insert(user.clone(), preferences.clone());
        Ok(preferences)
    }

    /// The global default preferences, if they exist.
    pub(crate) async fn global_preferences(&mut self) -> Result<Option<Map<String, Value>>> {
        if let Some(Loaded(preferences)) = &self.global_preferences {
            return Ok(preferences.clone());
        }
        let sql = format!(
            "SELECT preferences FROM {} WHERE builtinKey = ?",
            self.database.table("user", "user_preferences")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(GLOBAL_PREFERENCES)
            .fetch_optional(self.database.pool())
            .await?;
        let preferences = row
            .map(|row| preference_map(rows::json(&row, "preferences")?))
            .transpose()?;
        self.global_preferences = Some(Loaded(preferences.clone()));
        Ok(preferences)
    }

    /// `PhabricatorUserStore::fetchProject()`, `None` if it doesn't exist.
    pub(crate) async fn project(&mut self, phid: &Phid) -> Result<Option<ProjectInfo>> {
        if let Some(project) = self.projects.get(phid) {
            return Ok(project.clone());
        }
        let info = load_project(self.database, self.config, self.now, phid).await?;
        self.projects.insert(phid.clone(), info.clone());
        Ok(info)
    }

    /// The PHID of the `secure-revision` project (`executeOne()`).
    pub(crate) async fn secure_project(&mut self) -> Result<Phid> {
        if let Some(phid) = &self.secure_project {
            return Ok(phid.clone());
        }
        let projects =
            phab_app_project::storage::load_by_name(self.database, "secure-revision").await?;
        let [project] = projects.as_slice() else {
            return Err(Fail::Unsupported("no single secure-revision project"));
        };
        if project.parent_phid.is_some() {
            return Err(Fail::Unsupported("a secure-revision subproject"));
        }
        self.secure_project = Some(project.phid.clone());
        Ok(project.phid.clone())
    }

    /// Destinations of the edges of one type from `source`, newest first.
    pub(crate) async fn edge_destinations(
        &self,
        source: &Phid,
        edge_type: i64,
    ) -> Result<Vec<Phid>> {
        Ok(edges::load_destination_phids(
            self.database,
            std::slice::from_ref(source),
            &[edge_type],
            None,
        )
        .await?)
    }

    /// The revisions with these PHIDs.
    pub(crate) async fn revisions(&self, phids: &[Phid]) -> Result<Vec<RevisionRow>> {
        if phids.is_empty() {
            return Ok(Vec::new());
        }
        let database = self.database;
        let sql = format!(
            "SELECT r.id, r.phid, r.title, r.status, r.authorPHID, r.repositoryPHID, \
             r.activeDiffPHID, r.properties, d.id AS diffID FROM {} r LEFT JOIN {} d \
             ON d.phid = r.activeDiffPHID WHERE r.phid IN ({})",
            database.table("differential", "differential_revision"),
            database.table("differential", "differential_diff"),
            phab_db::placeholders(phids.len()),
        );
        let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phid_strings(phids))
            .fetch_all(database.pool())
            .await?;
        let mut revisions = Vec::new();
        for row in &found {
            revisions.push(RevisionRow {
                id: rows::int(row, "id")?,
                phid: Phid::new(rows::text(row, "phid")?),
                title: required_text(row, "title")?,
                status: rows::text(row, "status")?,
                author_phid: rows::opt_phid(row, "authorPHID")?,
                repository_phid: rows::opt_phid(row, "repositoryPHID")?,
                active_diff_phid: rows::opt_phid(row, "activeDiffPHID")?,
                active_diff_id: rows::opt_int(row, "diffID")?,
                properties: rows::json(row, "properties")?,
                reviewers: Vec::new(),
            });
        }
        let sql = format!(
            "SELECT revisionPHID, reviewerPHID, reviewerStatus, voidedPHID FROM {} \
             WHERE revisionPHID IN ({}) ORDER BY id ASC",
            database.table("differential", "differential_reviewer"),
            phab_db::placeholders(phids.len()),
        );
        let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phid_strings(phids))
            .fetch_all(database.pool())
            .await?;
        for row in &found {
            let revision = Phid::new(rows::text(row, "revisionPHID")?);
            let reviewer = ReviewerRow {
                phid: Phid::new(rows::text(row, "reviewerPHID")?),
                status: rows::text(row, "reviewerStatus")?,
                voided: rows::opt_phid(row, "voidedPHID")?.is_some(),
            };
            if let Some(revision) = revisions.iter_mut().find(|item| item.phid == revision) {
                // mpull() by reviewer PHID keeps the last row of a PHID, at
                // the place of the first.
                match revision
                    .reviewers
                    .iter_mut()
                    .find(|item| item.phid == reviewer.phid)
                {
                    Some(existing) => *existing = reviewer,
                    None => revision.reviewers.push(reviewer),
                }
            }
        }
        Ok(revisions)
    }

    /// The transactions with these PHIDs.
    pub(crate) async fn transactions(&self, phids: &[Phid]) -> Result<Vec<TransactionRow>> {
        if phids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, phid, authorPHID, objectPHID, commentPHID, transactionType, oldValue, \
             newValue, metadata FROM {} WHERE phid IN ({})",
            self.database
                .table("differential", "differential_transaction"),
            phab_db::placeholders(phids.len()),
        );
        let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phid_strings(phids))
            .fetch_all(self.database.pool())
            .await?;
        found
            .iter()
            .map(|row| {
                Ok(TransactionRow {
                    id: rows::int(row, "id")?,
                    phid: Phid::new(rows::text(row, "phid")?),
                    author_phid: Phid::new(rows::text(row, "authorPHID")?),
                    object_phid: Phid::new(rows::text(row, "objectPHID")?),
                    comment_phid: rows::opt_phid(row, "commentPHID")?,
                    transaction_type: rows::text(row, "transactionType")?,
                    old_value: json_column(row, "oldValue")?,
                    new_value: json_column(row, "newValue")?,
                    metadata: json_column(row, "metadata")?,
                })
            })
            .collect()
    }

    /// The comment with this PHID; `inline` keeps only inline comments
    /// (`DifferentialDiffInlineCommentQuery`).
    pub(crate) async fn comment(&self, phid: &Phid, inline: bool) -> Result<Option<CommentRow>> {
        let condition = if inline {
            " AND changesetID IS NOT NULL"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, authorPHID, content, changesetID, isNewFile, lineNumber, lineLength, \
             replyToCommentPHID, attributes, dateCreated FROM {} WHERE phid = ?{condition}",
            self.database
                .table("differential", "differential_transaction_comment"),
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(phid.as_str())
            .fetch_optional(self.database.pool())
            .await?;
        row.map(|row| {
            Ok(CommentRow {
                id: rows::int(&row, "id")?,
                author_phid: Phid::new(rows::text(&row, "authorPHID")?),
                content: required_text(&row, "content")?,
                changeset_id: rows::opt_int(&row, "changesetID")?,
                is_new_file: rows::flag(&row, "isNewFile")?,
                line_number: rows::int(&row, "lineNumber")?,
                line_length: rows::int(&row, "lineLength")?,
                reply_to_phid: rows::opt_phid(&row, "replyToCommentPHID")?,
                attributes: json_column(&row, "attributes")?,
                date_created: rows::int(&row, "dateCreated")?,
            })
        })
        .transpose()
    }

    /// The raw custom field value of `DifferentialBugzillaBugIDField`.
    pub(crate) async fn bug_field(&self, revision: &Phid) -> Result<Option<String>> {
        let sql = format!(
            "SELECT fieldValue FROM {} WHERE objectPHID = ? AND fieldIndex = ?",
            self.database
                .table("differential", "differential_customfieldstorage")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(revision.as_str())
            .bind(phab_core::hash::digest_for_index(
                b"differential:bugzilla-bug-id",
            ))
            .fetch_optional(self.database.pool())
            .await?;
        row.map(|row| required_text(&row, "fieldValue")).transpose()
    }

    /// The repository with this PHID or ID.
    pub(crate) async fn repository(
        &self,
        phid: Option<&Phid>,
        id: Option<i64>,
    ) -> Result<Option<RepositoryRow>> {
        let (column, value) = match (phid, id) {
            (Some(phid), _) => ("phid", phid.as_str().to_owned()),
            (None, Some(id)) => ("id", id.to_string()),
            (None, None) => return Ok(None),
        };
        let sql = format!(
            "SELECT phid, name, versionControlSystem FROM {} WHERE {column} = ?",
            self.database.table("repository", "repository")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(value)
            .fetch_optional(self.database.pool())
            .await?;
        row.map(|row| {
            Ok(RepositoryRow {
                phid: Phid::new(rows::text(&row, "phid")?),
                name: required_text(&row, "name")?,
                vcs: rows::text(&row, "versionControlSystem")?,
            })
        })
        .transpose()
    }

    /// `DiffusionCommitQuery`: the commit, if it and its repository exist.
    pub(crate) async fn commit(&self, phid: &str) -> Result<Option<CommitRow>> {
        let sql = format!(
            "SELECT c.commitIdentifier, c.repositoryID FROM {} c JOIN {} r ON r.id = c.repositoryID \
             WHERE c.phid = ?",
            self.database.table("repository", "repository_commit"),
            self.database.table("repository", "repository"),
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(phid)
            .fetch_optional(self.database.pool())
            .await?;
        row.map(|row| {
            Ok(CommitRow {
                identifier: required_text(&row, "commitIdentifier")?,
                repository_id: rows::int(&row, "repositoryID")?,
            })
        })
        .transpose()
    }

    /// The `uri` of the repository's URIs whose I/O type is `observe`, in
    /// the order of `PhabricatorRepositoryURIQuery`.
    pub(crate) async fn observed_uris(&self, repository: &Phid) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT uri FROM {} WHERE repositoryPHID = ? AND ioType = 'observe' ORDER BY id DESC",
            self.database.table("repository", "repository_uri")
        );
        let found = phab_db::sqlx::query(&sql)
            .bind(repository.as_str())
            .fetch_all(self.database.pool())
            .await?;
        found.iter().map(|row| required_text(row, "uri")).collect()
    }

    /// `PhabricatorDiffStore::find()`; `None` if the diff doesn't exist.
    pub(crate) async fn diff(&mut self, phid: &Phid) -> Result<Option<DiffRow>> {
        if let Some(diff) = self.diffs.get(phid) {
            return Ok(diff.clone());
        }
        let database = self.database;
        let sql = format!(
            "SELECT id FROM {} WHERE phid = ?",
            database.table("differential", "differential_diff")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(phid.as_str())
            .fetch_optional(database.pool())
            .await?;
        let diff = match row {
            None => None,
            Some(row) => {
                let id = rows::int(&row, "id")?;
                Some(DiffRow {
                    id,
                    changesets: load_raw_changesets(database, id).await?,
                })
            }
        };
        self.diffs.insert(phid.clone(), diff.clone());
        Ok(diff)
    }

    /// `DifferentialChangesetQuery` by ID with `needHunks()`.
    pub(crate) async fn changeset(&self, id: i64) -> Result<Option<phab_diff::Changeset>> {
        let sql = format!(
            "SELECT diffID FROM {} WHERE id = ?",
            self.database
                .table("differential", "differential_changeset")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.database.pool())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let diff_id = rows::int(&row, "diffID")?;
        let changesets = phab_diff::storage::load_changesets(self.database, diff_id)
            .await
            .map_err(|_| Fail::Unsupported("a changeset Rust can't load"))?;
        Ok(changesets.into_iter().find(|changeset| changeset.id == id))
    }
}

/// A JSON column, as PHP decodes it.
fn json_column(row: &MySqlRow, column: &str) -> Result<Value> {
    let text = strict_text(row, column)?;
    match text.as_deref() {
        None | Some("") => Ok(Value::Null),
        Some(text) => {
            serde_json::from_str(text).map_err(|_| Fail::Unsupported("invalid JSON in a column"))
        }
    }
}

/// Stored preferences: a dictionary, or an empty array.
fn preference_map(value: Value) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        Value::Array(items) if items.is_empty() => Ok(Map::new()),
        _ => Err(Fail::Unsupported("unusual stored preferences")),
    }
}

/// The changesets of a diff, newest first, with the stored data of their
/// hunks in ID order.
async fn load_raw_changesets(database: &Database, diff_id: i64) -> Result<Vec<RawChangeset>> {
    let sql = format!(
        "SELECT id, filename, changeType, fileType, metadata, newProperties FROM {} \
         WHERE diffID = ? ORDER BY id DESC",
        database.table("differential", "differential_changeset")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .fetch_all(database.pool())
        .await?;
    let mut ids = Vec::new();
    let mut changesets = Vec::new();
    for row in &found {
        ids.push(rows::int(row, "id")?);
        changesets.push(RawChangeset {
            filename: required_text(row, "filename")?,
            file_type: rows::int(row, "fileType")?,
            change_type: rows::int(row, "changeType")?,
            metadata: json_column(row, "metadata")?,
            new_properties: json_column(row, "newProperties")?,
            hunk_data: Vec::new(),
        });
    }
    if ids.is_empty() {
        return Ok(changesets);
    }
    let sql = format!(
        "SELECT changesetID, data FROM {} WHERE changesetID IN ({}) ORDER BY id",
        database.table("differential", "differential_hunk"),
        phab_db::placeholders(ids.len()),
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
        .fetch_all(database.pool())
        .await?;
    for row in &found {
        use phab_db::sqlx::Row;
        let changeset = rows::int(row, "changesetID")?;
        let data: Vec<u8> = row
            .try_get::<Vec<u8>, _>("data")
            .map_err(|error| Fail::Internal(error.to_string()))?;
        if let Some(index) = ids.iter().position(|id| *id == changeset) {
            changesets[index].hunk_data.push(data);
        }
    }
    Ok(changesets)
}
