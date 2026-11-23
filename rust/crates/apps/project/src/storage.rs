//! `PhabricatorProject` and `PhabricatorProjectSlug` rows.

use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};

/// `PhabricatorProjectStatus::STATUS_ARCHIVED`.
const STATUS_ARCHIVED: &str = "100";
/// `PhabricatorProjectStatus::STATUS_ACTIVE`.
pub(crate) const STATUS_ACTIVE: &str = "0";

const COLUMNS: &str = "p.id, p.phid, p.name, p.status, p.icon, p.color, p.viewPolicy, p.editPolicy, \
     p.joinPolicy, p.parentProjectPHID, p.milestoneNumber, p.primarySlug, p.spacePHID";

/// A project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The row ID.
    pub id: i64,
    /// The project PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// The status: `0` active, `100` archived.
    pub status: String,
    /// The icon key.
    pub icon: String,
    /// The color key.
    pub color: String,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The join policy.
    pub join_policy: String,
    /// The parent, for subprojects and milestones.
    pub parent_phid: Option<Phid>,
    /// Set for milestones.
    pub milestone_number: Option<i64>,
    /// The hashtag, without `#`.
    pub primary_slug: Option<String>,
    /// The space.
    pub space_phid: Option<Phid>,
}

impl Project {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            status: rows::text(row, "status")?,
            icon: rows::text(row, "icon")?,
            color: rows::text(row, "color")?,
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            join_policy: rows::text(row, "joinPolicy")?,
            parent_phid: rows::opt_phid(row, "parentProjectPHID")?,
            milestone_number: rows::opt_int(row, "milestoneNumber")?,
            primary_slug: rows::opt_text(row, "primarySlug")?.filter(|slug| !slug.is_empty()),
            space_phid: rows::opt_phid(row, "spacePHID")?,
        })
    }

    /// `isMilestone()`.
    pub fn is_milestone(&self) -> bool {
        self.milestone_number.is_some()
    }

    /// `isArchived()`.
    pub fn is_archived(&self) -> bool {
        self.status == STATUS_ARCHIVED
    }

    /// The handle URI: `/tag/<slug>/`, or `/project/view/<id>/` without a
    /// hashtag (`PhabricatorProjectProjectPHIDType::loadHandles()`).
    pub fn uri(&self) -> String {
        match &self.primary_slug {
            Some(slug) => format!("/tag/{slug}/"),
            None => format!("/project/view/{}/", self.id),
        }
    }

    /// `getDisplayName()`: milestones show as "Parent (Milestone)".
    pub fn display_name(&self, parent: Option<&Self>) -> String {
        match parent {
            Some(parent) if self.is_milestone() => format!("{} ({})", parent.name, self.name),
            _ => self.name.clone(),
        }
    }
}

/// The projects with these PHIDs; unknown PHIDs are skipped.
pub async fn load_by_phid(database: &Database, phids: &[Phid]) -> Result<Vec<Project>, DbError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM {} p WHERE p.phid IN ({})",
        database.table("project", "project"),
        phab_db::placeholders(phids.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(Project::from_row).collect()
}

/// `PhabricatorObjectHasWatcherEdgeType`.
pub const OBJECT_HAS_WATCHER: i64 = 47;
/// `PhabricatorProjectProjectHasObjectEdgeType`.
pub const PROJECT_HAS_OBJECT: i64 = 42;
/// The custom field key of `PhabricatorProjectDescriptionField`.
const DESCRIPTION_FIELD: &str = "std:project:internal:description";

/// The projects with this name (`withNames()`, compared without case by
/// MySQL's collation).
pub async fn load_by_name(database: &Database, name: &str) -> Result<Vec<Project>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM {} p WHERE p.name = ?",
        database.table("project", "project")
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(name)
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(Project::from_row).collect()
}

/// The project with this ID, if any.
pub async fn load_by_id(database: &Database, id: i64) -> Result<Option<Project>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM {} p WHERE p.id = ?",
        database.table("project", "project")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?;
    row.as_ref().map(Project::from_row).transpose()
}

/// The subprojects and milestones of `parent`.
pub async fn load_children(database: &Database, parent: &Phid) -> Result<Vec<Project>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM {} p WHERE p.parentProjectPHID = ? ORDER BY p.milestoneNumber, p.name",
        database.table("project", "project")
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(parent.as_str())
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(Project::from_row).collect()
}

/// The description (`PhabricatorProjectDescriptionField`), from the
/// custom field storage.
pub async fn load_description(database: &Database, project: &Phid) -> Result<String, DbError> {
    Ok(load_stored_description(database, project)
        .await?
        .unwrap_or_default())
}

/// The stored description, `None` without a storage row.
pub async fn load_stored_description(
    database: &Database,
    project: &Phid,
) -> Result<Option<String>, DbError> {
    let sql = format!(
        "SELECT fieldValue FROM {} WHERE objectPHID = ? AND fieldIndex = ?",
        database.table("project", "project_customfieldstorage")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(project.as_str())
        .bind(phab_core::hash::digest_for_index(
            DESCRIPTION_FIELD.as_bytes(),
        ))
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| rows::text(&row, "fieldValue")).transpose()
}

/// The destinations of a project's edges of one type, like members
/// (`PROJECT_HAS_MEMBER`) or watchers ([`OBJECT_HAS_WATCHER`]).
pub async fn load_edge_destinations(
    database: &Database,
    project: &Phid,
    edge_type: i64,
) -> Result<Vec<Phid>, DbError> {
    phab_db::edges::load_destination_phids(
        database,
        std::slice::from_ref(project),
        &[edge_type],
        None,
    )
    .await
}

/// The projects `user` is a direct member of (`withMemberPHIDs()`).
pub async fn load_member_projects(
    database: &Database,
    user: &Phid,
) -> Result<Vec<Project>, DbError> {
    let phids = phab_db::edges::load_destination_phids(
        database,
        std::slice::from_ref(user),
        &[phab_db::edges::MEMBER_OF_PROJECT],
        None,
    )
    .await?;
    load_by_phid(database, &phids).await
}

/// Up to `limit` projects with a hashtag starting with `prefix`, with the
/// first such hashtag of each; archived projects are left out, as the
/// project datasource does by default. Not filtered by policy.
pub async fn search_by_slug_prefix(
    database: &Database,
    prefix: &str,
    limit: u32,
) -> Result<Vec<(String, Project)>, DbError> {
    let sql = format!(
        "SELECT MIN(s.slug) AS slug, {COLUMNS} FROM {} s JOIN {} p ON p.phid = s.projectPHID \
         WHERE s.slug LIKE ? AND p.status != ? GROUP BY p.id ORDER BY slug LIMIT ?",
        database.table("project", "project_slug"),
        database.table("project", "project"),
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(format!("{}%", phab_db::like_escape(&prefix.to_lowercase())))
        .bind(STATUS_ARCHIVED)
        .bind(limit)
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok((rows::text(row, "slug")?, Project::from_row(row)?)))
        .collect()
}

/// The projects with these hashtags, as `(slug, project)` pairs. Slugs are
/// matched exactly: callers lowercase them first, as
/// `PhabricatorProjectProjectPHIDType::loadNamedObjects()` does.
pub async fn load_by_slug(
    database: &Database,
    slugs: &[String],
) -> Result<Vec<(String, Project)>, DbError> {
    if slugs.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT s.slug, {COLUMNS} FROM {} s JOIN {} p ON p.phid = s.projectPHID WHERE s.slug IN ({})",
        database.table("project", "project_slug"),
        database.table("project", "project"),
        phab_db::placeholders(slugs.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), slugs.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok((rows::text(row, "slug")?, Project::from_row(row)?)))
        .collect()
}
