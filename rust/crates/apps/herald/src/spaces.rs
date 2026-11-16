//! Spaces (`PhabricatorSpacesNamespace`), for `S1` references.

use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, PolicySubject, Viewer};

use crate::VisibilityError;

/// A `spaces_namespace` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    /// The row ID.
    pub id: i64,
    /// The space PHID.
    pub phid: Phid,
    /// `namespaceName`.
    pub name: String,
    /// `isArchived`.
    pub is_archived: bool,
    view_policy: String,
    edit_policy: String,
}

impl Space {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "namespaceName")?,
            is_archived: rows::flag(row, "isArchived")?,
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
        })
    }

    /// `getMonogram()`.
    pub fn monogram(&self) -> String {
        format!("S{}", self.id)
    }

    fn subject(&self) -> PolicySubject {
        PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone())
    }
}

/// The spaces with these IDs; unknown IDs are skipped.
pub async fn load_by_ids(database: &Database, ids: &[i64]) -> Result<Vec<Space>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, phid, namespaceName, isArchived, viewPolicy, editPolicy FROM {} WHERE id IN ({})",
        database.table("spaces", "spaces_namespace"),
        phab_db::placeholders(ids.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), ids)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Space::from_row)
        .collect()
}

/// The spaces `viewer` can see (`PhabricatorSpacesNamespaceQuery`).
pub async fn visible(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    spaces: Vec<Space>,
    now: i64,
) -> Result<Vec<Space>, VisibilityError> {
    let subjects: Vec<PolicySubject> = spaces.iter().map(Space::subject).collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts =
        phab_policy::loader::load_facts(database, config, viewer, &refs, &NoExtendedObjects, now)
            .await?;
    let passed = phab_policy::filter(viewer, &refs, &[Capability::VIEW], &facts)?;
    Ok(spaces
        .into_iter()
        .zip(passed)
        .filter_map(|(space, visible)| visible.then_some(space))
        .collect())
}
