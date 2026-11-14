//! The repositories of revisions (`PhabricatorRepository`): what pages
//! show of them, and their policies.

use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::{Capability, ObjectKind, PolicySubject, Viewer};

use crate::storage::{PUSH, Repositories, VisibilityError};

/// A `repository` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Repository {
    /// The row ID.
    pub id: i64,
    /// The repository PHID.
    pub phid: Phid,
    /// The name, like "mozilla-central".
    pub name: String,
    /// The callsign, like `MOZILLACENTRAL`, if it has one.
    pub callsign: Option<String>,
    /// The short name (`repositorySlug`), if it has one.
    pub slug: Option<String>,
    subject: PolicySubject,
}

const COLUMNS: &str =
    "id, phid, name, callsign, repositorySlug, viewPolicy, editPolicy, pushPolicy, spacePHID";

impl Repository {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        let phid = Phid::new(rows::text(row, "phid")?);
        let mut subject = PolicySubject::new(Some(phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, rows::text(row, "viewPolicy")?)
            .with_policy(Capability::EDIT, rows::text(row, "editPolicy")?)
            .with_policy(PUSH, rows::text(row, "pushPolicy")?);
        subject.capabilities.push(PUSH);
        subject.space = Some(rows::opt_text(row, "spacePHID")?.map(Phid::new));
        let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
        Ok(Self {
            id: rows::int(row, "id")?,
            phid,
            name: rows::text(row, "name")?,
            callsign: nonempty(rows::opt_text(row, "callsign")?),
            slug: nonempty(rows::opt_text(row, "repositorySlug")?),
            subject,
        })
    }

    /// The repository as a policy object.
    pub fn subject(&self) -> &PolicySubject {
        &self.subject
    }

    /// `getMonogram()`: `rCALLSIGN`, or `R123` without a callsign.
    pub fn monogram(&self) -> String {
        match &self.callsign {
            Some(callsign) => format!("r{callsign}"),
            None => format!("R{}", self.id),
        }
    }

    /// `getDisplayName()`: the short name, or the monogram.
    pub fn display_name(&self) -> String {
        self.slug.clone().unwrap_or_else(|| self.monogram())
    }
}

/// The repositories with these PHIDs; unknown PHIDs are skipped.
pub async fn load(database: &Database, phids: &[Phid]) -> Result<Vec<Repository>, DbError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE phid IN ({})",
        database.table("repository", "repository"),
        phab_db::placeholders(phids.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Repository::from_row)
        .collect()
}

/// The repositories `viewer` can see (`PhabricatorRepositoryQuery`).
pub async fn visible(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    repositories: Vec<Repository>,
    now: i64,
) -> Result<Vec<Repository>, VisibilityError> {
    let refs: Vec<&PolicySubject> = repositories.iter().map(Repository::subject).collect();
    let facts = phab_policy::loader::load_facts(
        database,
        config,
        viewer,
        &refs,
        &Repositories { database },
        now,
    )
    .await?;
    let passed = phab_policy::filter(viewer, &refs, &[Capability::VIEW], &facts)?;
    Ok(repositories
        .into_iter()
        .zip(passed)
        .filter_map(|(repository, visible)| visible.then_some(repository))
        .collect())
}
