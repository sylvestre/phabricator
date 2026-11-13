//! `PhabricatorProject` and `PhabricatorProjectSlug` rows.

use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};

/// `PhabricatorProjectStatus::STATUS_ARCHIVED`.
const STATUS_ARCHIVED: &str = "100";

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
