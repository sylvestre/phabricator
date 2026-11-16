//! Commits (`PhabricatorRepositoryCommit`), for references to them
//! (`rMOZabcdef`, `R12:abcdef`, bare hashes), loaded like
//! `DiffusionCommitQuery::withIdentifiers()`.

use std::collections::HashMap;

use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::Viewer;

use crate::repository::{self, Repository};
use crate::storage::VisibilityError;

/// `PhabricatorRepository::MINIMUM_UNQUALIFIED_HASH`.
const MINIMUM_UNQUALIFIED_HASH: usize = 7;
/// `PhabricatorRepository::MINIMUM_QUALIFIED_HASH`.
const MINIMUM_QUALIFIED_HASH: usize = 5;
/// `PhabricatorRepositoryCommit::IMPORTED_UNREACHABLE`.
const IMPORTED_UNREACHABLE: i64 = 2048;

/// A `repository_commit` row, with its repository.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    /// The commit PHID.
    pub phid: Phid,
    /// The hash, or the SVN revision.
    pub identifier: String,
    /// The first line of the message.
    pub summary: String,
    /// `isUnreachable()`: the commit was rewritten or removed.
    pub is_unreachable: bool,
    /// Its repository.
    pub repository: Repository,
}

impl Commit {
    /// `formatCommitName()`, the handle's name.
    pub fn name(&self) -> String {
        self.repository.format_commit_name(&self.identifier)
    }

    /// The handle's full name: the name and the summary.
    pub fn full_name(&self) -> String {
        if self.summary.is_empty() {
            self.name()
        } else {
            format!("{}: {}", self.name(), self.summary)
        }
    }

    /// `getURI()`.
    pub fn uri(&self) -> String {
        self.repository.commit_uri(&self.identifier)
    }
}

/// A commit reference split into its repository and its hash.
struct Reference {
    written: String,
    repository: Option<String>,
    hash: String,
}

/// `/^(?:[rR]([A-Z]+:?|[0-9]+:))?(.*)$/`.
fn split(identifier: &str) -> Reference {
    let bytes = identifier.as_bytes();
    let mut repository = None;
    let mut rest = identifier;
    if let Some((b'r' | b'R', tail)) = bytes.split_first() {
        let letters = tail
            .iter()
            .take_while(|byte| byte.is_ascii_uppercase())
            .count();
        let digits = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if letters > 0 {
            let colon = usize::from(tail.get(letters) == Some(&b':'));
            repository = Some(identifier[1..=letters].to_owned());
            rest = &identifier[1 + letters + colon..];
        } else if digits > 0 && tail.get(digits) == Some(&b':') {
            repository = Some(identifier[1..=digits].to_owned());
            rest = &identifier[2 + digits..];
        }
    }
    Reference {
        written: identifier.to_owned(),
        repository: repository.filter(|name| !name.is_empty() && name != "0"),
        hash: rest.to_owned(),
    }
}

fn from_row(row: &MySqlRow) -> Result<(i64, Phid, String, String, i64), DbError> {
    Ok((
        rows::int(row, "repositoryID")?,
        Phid::new(rows::text(row, "phid")?),
        rows::text(row, "commitIdentifier")?,
        rows::text(row, "summary")?,
        rows::int(row, "importStatus")?,
    ))
}

/// The commits named by `identifiers` that `viewer` can see, by
/// identifier; an identifier matching several commits names none.
pub async fn load_visible_by_identifiers(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    identifiers: &[String],
    now: i64,
) -> Result<HashMap<String, Commit>, VisibilityError> {
    let references: Vec<Reference> = identifiers
        .iter()
        .map(|identifier| split(identifier))
        .collect();
    let scopes = visible_scopes(database, config, viewer, &references, now).await?;
    let Some((clauses, values)) = where_clauses(&references, &scopes) else {
        return Ok(HashMap::new());
    };
    let sql = format!(
        "SELECT repositoryID, phid, commitIdentifier, summary, importStatus FROM {} WHERE {}",
        database.table("repository", "repository_commit"),
        clauses.join(" OR "),
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), values)
        .fetch_all(database.pool())
        .await
        .map_err(|error| db_error(error.into()))?
        .iter()
        .map(from_row)
        .collect::<Result<Vec<_>, DbError>>()
        .map_err(db_error)?;

    // Commits are visible to whoever sees their repository.
    let mut repository_ids: Vec<i64> = found.iter().map(|(id, ..)| *id).collect();
    repository_ids.sort_unstable();
    repository_ids.dedup();
    let repositories = repository::load_by_ids(database, &repository_ids)
        .await
        .map_err(db_error)?;
    let repositories: HashMap<i64, Repository> =
        repository::visible(database, config, viewer, repositories, now)
            .await?
            .into_iter()
            .map(|repository| (repository.id, repository))
            .collect();
    let commits =
        found
            .into_iter()
            .filter_map(|(repository_id, phid, identifier, summary, status)| {
                Some(Commit {
                    phid,
                    identifier,
                    summary,
                    is_unreachable: status & IMPORTED_UNREACHABLE != 0,
                    repository: repositories.get(&repository_id)?.clone(),
                })
            });
    Ok(identifier_map(&references, commits))
}

/// The repositories the viewer can see which references name, by callsign
/// and by ID.
async fn visible_scopes(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    references: &[Reference],
    now: i64,
) -> Result<HashMap<String, Repository>, VisibilityError> {
    let scopes: Vec<String> = references
        .iter()
        .filter_map(|reference| reference.repository.clone())
        .collect();
    // A callsign is an identifier too; numbers are IDs.
    let repositories = repository::load_by_identifiers(database, &scopes)
        .await
        .map_err(db_error)?;
    Ok(repository::visible(
        database,
        config,
        viewer,
        repositories.into_values().collect(),
        now,
    )
    .await?
    .into_iter()
    .flat_map(|repository| {
        let mut keys = vec![repository.id.to_string()];
        keys.extend(repository.callsign.clone());
        keys.into_iter().map(move |key| (key, repository.clone()))
    })
    .collect())
}

/// The conditions finding the commits of `references`, with their
/// values, or `None` if no reference can name a commit.
fn where_clauses(
    references: &[Reference],
    scopes: &HashMap<String, Repository>,
) -> Option<(Vec<&'static str>, Vec<String>)> {
    let mut clauses = Vec::new();
    let mut values: Vec<String> = Vec::new();
    for reference in references {
        let Some(scope) = &reference.repository else {
            if reference.hash.len() >= MINIMUM_UNQUALIFIED_HASH {
                clauses.push("(commitIdentifier LIKE ? AND LENGTH(commitIdentifier) = 40)");
                values.push(format!("{}%", like_escape(&reference.hash)));
            }
            continue;
        };
        let Some(repository) = scopes.get(scope) else {
            continue;
        };
        if repository.is_svn() {
            let digits = !reference.hash.is_empty()
                && reference.hash.bytes().all(|byte| byte.is_ascii_digit());
            if digits {
                clauses.push("(repositoryID = ? AND commitIdentifier = ?)");
                values.push(repository.id.to_string());
                values.push(reference.hash.trim_start_matches('0').to_owned());
            }
        } else if reference.hash.len() >= MINIMUM_QUALIFIED_HASH {
            clauses.push("(repositoryID = ? AND commitIdentifier LIKE ?)");
            values.push(repository.id.to_string());
            values.push(format!("{}%", like_escape(&reference.hash)));
        }
    }
    (!clauses.is_empty()).then_some((clauses, values))
}

/// `getIdentifierMap()`: which reference names which commit, keeping
/// only the references naming one.
fn identifier_map(
    references: &[Reference],
    commits: impl Iterator<Item = Commit>,
) -> HashMap<String, Commit> {
    let mut matches: HashMap<String, Vec<Commit>> = HashMap::new();
    for commit in commits {
        let repository = &commit.repository;
        let mut prefixes = vec![format!("R{}:", repository.id), String::new()];
        if let Some(callsign) = &repository.callsign {
            prefixes.push(format!("r{callsign}"));
            prefixes.push(format!("r{callsign}:"));
        }
        let lengths: Vec<usize> = if repository.is_svn() {
            vec![commit.identifier.len()]
        } else {
            (MINIMUM_QUALIFIED_HASH..=commit.identifier.len()).collect()
        };
        for length in lengths {
            let part = &commit.identifier[..length];
            for prefix in &prefixes {
                let key = format!("{prefix}{part}");
                if references.iter().any(|reference| reference.written == key) {
                    matches.entry(key).or_default().push(commit.clone());
                }
            }
        }
    }
    matches
        .into_iter()
        .filter_map(|(identifier, mut commits)| {
            (commits.len() == 1).then(|| (identifier, commits.remove(0)))
        })
        .collect()
}

fn db_error(error: DbError) -> VisibilityError {
    VisibilityError::Loader(error.into())
}

/// Escapes `%`, `_` and `\` for `LIKE`, as `qsprintf()`'s `%>` does.
fn like_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_split_like_php() {
        let split = |text: &str| {
            let reference = split(text);
            (reference.repository, reference.hash)
        };
        assert_eq!(
            split("rMOZabcdef"),
            (Some("MOZ".to_owned()), "abcdef".to_owned())
        );
        assert_eq!(
            split("rMOZ:abcdef"),
            (Some("MOZ".to_owned()), "abcdef".to_owned())
        );
        assert_eq!(
            split("R12:abcdef"),
            (Some("12".to_owned()), "abcdef".to_owned())
        );
        assert_eq!(split("abcdef1"), (None, "abcdef1".to_owned()));
        assert_eq!(split("R12abc"), (None, "R12abc".to_owned()));
    }
}
