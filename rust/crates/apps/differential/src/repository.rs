//! The repositories of revisions (`PhabricatorRepository`): what pages
//! show of them, and their policies.

use std::collections::HashMap;

use phab_core::{Config, Phid, php};
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
    /// `versionControlSystem`: `git` or `hg` (Subversion is not ported).
    pub vcs: String,
    subject: PolicySubject,
    details: serde_json::Map<String, serde_json::Value>,
}

const COLUMNS: &str = "id, phid, name, callsign, repositorySlug, versionControlSystem, details, \
     viewPolicy, editPolicy, pushPolicy, spacePHID";

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
            vcs: rows::text(row, "versionControlSystem")?,
            subject,
            details: match rows::json(row, "details") {
                Ok(serde_json::Value::Object(details)) => details,
                _ => serde_json::Map::new(),
            },
        })
    }

    /// `getDetail()`: a value of the `details` column.
    pub fn detail(&self, key: &str) -> Option<&serde_json::Value> {
        self.details.get(key)
    }

    /// `getDefaultBranch()` of repositories with branches
    /// (`supportsRefs()`): the configured one, or Git's and Mercurial's.
    pub fn default_branch(&self) -> Option<String> {
        match self
            .details
            .get("default-branch")
            .and_then(|value| value.as_str())
        {
            Some(branch) if !branch.is_empty() => Some(branch.to_owned()),
            _ => match self.vcs.as_str() {
                "git" => Some("master".to_owned()),
                "hg" => Some("default".to_owned()),
                _ => None,
            },
        }
    }

    /// `isTracked()`: the status is "active".
    pub fn is_tracked(&self) -> bool {
        match self.details.get("tracking-enabled") {
            Some(serde_json::Value::String(status)) if status == "active" => true,
            Some(serde_json::Value::String(status)) if status == "inactive" => false,
            Some(value) => php::is_truthy(value),
            None => false,
        }
    }

    /// `getURI()`: `/source/<short name>/`, or `/diffusion/<callsign or ID>/`.
    pub fn uri(&self) -> String {
        if let Some(slug) = &self.slug {
            return format!("/source/{slug}/");
        }
        match &self.callsign {
            Some(callsign) => format!("/diffusion/{callsign}/"),
            None => format!("/diffusion/{}/", self.id),
        }
    }

    /// `getCommitURI()`.
    pub fn commit_uri(&self, identifier: &str) -> String {
        match &self.callsign {
            Some(callsign) => format!("/r{callsign}{identifier}"),
            None => format!("/R{}:{identifier}", self.id),
        }
    }

    /// `formatCommitName()`: Git and Mercurial hashes are cut to 12
    /// characters, and scoped by the monogram.
    pub fn format_commit_name(&self, identifier: &str) -> String {
        let name = if self.vcs == "git" || self.vcs == "hg" {
            identifier.get(..12).unwrap_or(identifier)
        } else {
            identifier
        };
        match &self.callsign {
            Some(callsign) => format!("r{callsign}{name}"),
            None => format!("R{}:{name}", self.id),
        }
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

/// What an identifier of `PhabricatorRepositoryQuery::withIdentifiers()`
/// is.
enum Identifier {
    Id(i64),
    Monogram,
    Phid,
    Callsign,
    Slug,
}

/// `/^(r[A-Z]+|R[1-9]\d*)\z/`.
fn is_monogram(identifier: &str) -> bool {
    let bytes = identifier.as_bytes();
    match bytes.split_first() {
        Some((b'r', rest)) => !rest.is_empty() && rest.iter().all(u8::is_ascii_uppercase),
        Some((b'R', rest)) => {
            rest.first()
                .is_some_and(|first| (b'1'..=b'9').contains(first))
                && rest.iter().all(u8::is_ascii_digit)
        }
        _ => false,
    }
}

fn identifier_kind(identifier: &str) -> Identifier {
    if !identifier.is_empty() && identifier.bytes().all(|byte| byte.is_ascii_digit()) {
        return identifier.parse().map_or(Identifier::Slug, Identifier::Id);
    }
    if is_monogram(identifier) {
        return Identifier::Monogram;
    }
    if Phid::new(identifier).phid_type() == phab_core::PhidType::REPOSITORY {
        return Identifier::Phid;
    }
    if !identifier.is_empty() && identifier.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Identifier::Callsign;
    }
    Identifier::Slug
}

/// The repositories named by these identifiers (IDs, monograms like
/// `rMOZ` or `R12`, PHIDs, callsigns or short names), by identifier
/// (`getIdentifierMap()`). Short names match without case.
pub async fn load_by_identifiers(
    database: &Database,
    identifiers: &[String],
) -> Result<HashMap<String, Repository>, DbError> {
    if identifiers.is_empty() {
        return Ok(HashMap::new());
    }
    let mut ids = Vec::new();
    let mut callsigns = Vec::new();
    let mut phids = Vec::new();
    let mut slugs = Vec::new();
    for identifier in identifiers {
        match identifier_kind(identifier) {
            Identifier::Id(id) => ids.push(id.to_string()),
            Identifier::Monogram => match identifier.strip_prefix('r') {
                Some(callsign) => callsigns.push(callsign.to_owned()),
                None => ids.push(identifier[1..].to_owned()),
            },
            Identifier::Phid => phids.push(identifier.clone()),
            Identifier::Callsign => callsigns.push(identifier.clone()),
            Identifier::Slug => slugs.push(identifier.to_lowercase()),
        }
    }
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    for (column, list) in [
        ("id", ids),
        ("callsign", callsigns),
        ("phid", phids),
        ("repositorySlug", slugs),
    ] {
        if !list.is_empty() {
            clauses.push(format!(
                "{column} IN ({})",
                phab_db::placeholders(list.len())
            ));
            values.extend(list);
        }
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE {}",
        database.table("repository", "repository"),
        clauses.join(" OR "),
    );
    let repositories: Vec<Repository> = phab_db::bind_all(phab_db::sqlx::query(&sql), values)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Repository::from_row)
        .collect::<Result<_, _>>()?;
    let mut map = HashMap::new();
    for identifier in identifiers {
        let found = repositories
            .iter()
            .find(|repository| match identifier_kind(identifier) {
                Identifier::Id(id) => repository.id == id && id.to_string() == *identifier,
                Identifier::Monogram => {
                    repository.monogram() == *identifier
                        || format!("R{}", repository.id) == *identifier
                }
                Identifier::Phid => repository.phid.as_str() == identifier,
                Identifier::Callsign => repository.callsign.as_deref() == Some(identifier.as_str()),
                Identifier::Slug => repository
                    .slug
                    .as_deref()
                    .is_some_and(|slug| slug.to_lowercase() == identifier.to_lowercase()),
            });
        if let Some(repository) = found {
            map.insert(identifier.clone(), repository.clone());
        }
    }
    Ok(map)
}

/// The repositories with these IDs; unknown IDs are skipped.
pub async fn load_by_ids(database: &Database, ids: &[i64]) -> Result<Vec<Repository>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE id IN ({})",
        database.table("repository", "repository"),
        phab_db::placeholders(ids.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), ids)
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
