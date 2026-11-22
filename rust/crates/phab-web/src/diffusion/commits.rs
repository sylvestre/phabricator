//! The commits of a repository the pages show (`DiffusionCommitQuery`
//! with `needCommitData()` and `needIdentities()`), and how they name
//! their authors.
//!
//! Commits are only loaded for a repository the viewer can see, whose
//! view policy is theirs.

use std::collections::HashMap;

use phab_core::{Phid, PhidType};
use phab_db::{Database, rows};
use serde_json::Value;

use crate::diff_html;
use crate::error::PageError;
use crate::harbormaster::Handle;

/// `PhabricatorRepositoryCommit::IMPORTED_MESSAGE | IMPORTED_CHANGE`.
const IMPORTED_PARTIAL: i64 = 1 | 2;
/// `PhabricatorRepositoryCommit::IMPORTED_ALL`.
pub(crate) const IMPORTED_ALL: i64 = 11;
/// `PhabricatorRepositoryCommit::IMPORTED_PERMANENT`.
pub(crate) const IMPORTED_PERMANENT: i64 = 1024;
/// `PhabricatorRepositoryCommit::IMPORTED_UNREACHABLE`.
pub(crate) const IMPORTED_UNREACHABLE: i64 = 2048;

/// A `repository_identity`.
#[derive(Debug, Clone)]
struct Identity {
    name: String,
    user: Option<Phid>,
}

/// A commit with its data and identities.
#[derive(Debug, Clone)]
pub(crate) struct Commit {
    pub(crate) id: i64,
    pub(crate) phid: Phid,
    pub(crate) identifier: String,
    pub(crate) epoch: i64,
    pub(crate) summary: String,
    pub(crate) audit_status: String,
    import_status: i64,
    author_identity: Option<(Phid, Identity)>,
    committer_identity: Option<(Phid, Identity)>,
    /// `repository_commitdata`: the author name, the message and the
    /// details.
    data: Option<(Option<String>, String, Value)>,
}

/// Who an author or committer is shown as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Person {
    /// A user, or an identity, by PHID (`getIdentityDisplayPHID()`).
    Phid(Phid),
    /// A raw name, like `alice <alice@example.com>`.
    Raw(String),
}

impl Commit {
    /// `isPartiallyImported()`.
    pub(crate) fn has_flags(&self, flags: i64) -> bool {
        self.import_status & flags == flags
    }

    /// The summary, or what `DiffusionCommitGraphView` shows of a commit
    /// still importing.
    pub(crate) fn description_html(&self) -> String {
        if self.has_flags(IMPORTED_PARTIAL) {
            diff_html::escape(&self.summary)
        } else {
            "<em>Importing…</em>".to_owned()
        }
    }

    /// The commit message.
    pub(crate) fn message(&self) -> Option<&str> {
        self.data.as_ref().map(|(_, message, _)| message.as_str())
    }

    fn detail(&self, key: &str) -> Option<&Value> {
        self.data
            .as_ref()
            .and_then(|(_, _, details)| details.get(key))
    }

    fn reference(&self, key: &str) -> Option<&str> {
        self.detail("ref")
            .and_then(|reference| reference.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    }

    /// `DiffusionCommitRef::formatUser()` of the ref's name and email.
    fn reference_user(&self, name: &str, email: &str) -> Option<String> {
        match (self.reference(name), self.reference(email)) {
            (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
            (None, Some(email)) => Some(email.to_owned()),
            (Some(name), None) => Some(name.to_owned()),
            (None, None) => None,
        }
    }

    /// `getAuthorString()`.
    fn author_string(&self) -> Option<String> {
        self.reference_user("authorName", "authorEmail")
            .or_else(|| {
                self.data
                    .as_ref()
                    .and_then(|(name, _, _)| name.clone())
                    .filter(|name| !name.is_empty())
            })
    }

    /// `getCommitterString()`.
    fn committer_string(&self) -> Option<String> {
        self.reference_user("committerName", "committerEmail")
            .or_else(|| {
                self.detail("committer")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|name| !name.is_empty())
    }

    /// `getAuthorEpoch()`.
    pub(crate) fn author_epoch(&self) -> Option<i64> {
        let value = self
            .detail("ref")
            .and_then(|reference| reference.get("authorEpoch"))
            .or_else(|| self.detail("authorEpoch"))?;
        let epoch = match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        }?;
        (epoch != 0).then_some(epoch)
    }

    fn display_phid(identity: Option<&(Phid, Identity)>) -> Option<Phid> {
        identity.map(|(phid, identity)| identity.user.clone().unwrap_or_else(|| phid.clone()))
    }

    /// `getAuthorDisplayPHID()`.
    fn author_display_phid(&self) -> Option<Phid> {
        Self::display_phid(self.author_identity.as_ref()).or_else(|| {
            self.detail("authorPHID")
                .and_then(Value::as_str)
                .map(Phid::new)
        })
    }

    /// `getCommitterDisplayPHID()`.
    fn committer_display_phid(&self) -> Option<Phid> {
        Self::display_phid(self.committer_identity.as_ref()).or_else(|| {
            self.detail("committerPHID")
                .and_then(Value::as_str)
                .map(Phid::new)
        })
    }

    /// The author, as `newCommitAuthorView()` shows them.
    pub(crate) fn author(&self) -> Option<Person> {
        self.author_display_phid()
            .map(Person::Phid)
            .or_else(|| self.author_string().map(Person::Raw))
    }

    /// The committer, as `newCommitCommitterView()` shows them.
    pub(crate) fn committer(&self) -> Option<Person> {
        self.committer_display_phid()
            .map(Person::Phid)
            .or_else(|| self.committer_string().map(Person::Raw))
    }

    /// `isAuthorSameAsCommitter()`.
    pub(crate) fn is_author_committer(&self) -> bool {
        match (self.author_display_phid(), self.committer_display_phid()) {
            (Some(author), Some(committer)) => author == committer,
            (None, None) => self.author_string() == self.committer_string(),
            _ => false,
        }
    }

    /// The user PHIDs of the people of the commit, for handles.
    pub(crate) fn user_phids(&self) -> Vec<Phid> {
        [self.author(), self.committer()]
            .into_iter()
            .flatten()
            .filter_map(|person| match person {
                Person::Phid(phid) if phid.phid_type() == PhidType::USER => Some(phid),
                _ => None,
            })
            .collect()
    }

    /// The name of an identity of this commit.
    fn identity_name(&self, phid: &Phid) -> Option<&str> {
        [&self.author_identity, &self.committer_identity]
            .into_iter()
            .flatten()
            .find(|(identity, _)| identity == phid)
            .map(|(_, identity)| identity.name.as_str())
    }

    /// A person as HTML: a user's handle, an identity's name, or
    /// `DiffusionView::renderName()` of a raw name.
    pub(crate) fn person_html(&self, person: &Person, handles: &HashMap<Phid, Handle>) -> String {
        match person {
            Person::Phid(phid) => match handles.get(phid) {
                Some(handle) => handle.link_html(),
                None => match self.identity_name(phid) {
                    Some(name) => render_name(name),
                    None => diff_html::escape("Unknown Object"),
                },
            },
            Person::Raw(name) => render_name(name),
        }
    }
}

/// `DiffusionView::renderName()`: the display name of an email address,
/// with the address as its tooltip, or the name as it is.
pub(crate) fn render_name(name: &str) -> String {
    let trimmed = name.trim();
    if let Some(open) = trimmed.rfind('<').filter(|_| trimmed.ends_with('>')) {
        let display = trimmed[..open].trim_matches(['\'', '"', ' ']);
        let address = &trimmed[open + 1..trimmed.len() - 1];
        if !display.is_empty() && address.contains('@') {
            return format!(
                "<span title=\"{}\">{}</span>",
                diff_html::escape(address),
                diff_html::escape(display)
            );
        }
    }
    diff_html::escape(name)
}

async fn load_identities(
    database: &Database,
    phids: &[String],
) -> Result<HashMap<String, Identity>, PageError> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT phid, identityNameRaw, currentEffectiveUserPHID FROM {} WHERE phid IN ({})",
        database.table("repository", "repository_identity"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let mut identities = HashMap::new();
    for row in &found {
        identities.insert(
            rows::text(row, "phid")?,
            Identity {
                name: rows::text(row, "identityNameRaw")?,
                user: rows::opt_text(row, "currentEffectiveUserPHID")?
                    .filter(|phid| !phid.is_empty())
                    .map(Phid::new),
            },
        );
    }
    Ok(identities)
}

async fn load_data(
    database: &Database,
    ids: &[i64],
) -> Result<HashMap<i64, (Option<String>, String, Value)>, PageError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT commitID, authorName, commitMessage, commitDetails FROM {} WHERE commitID IN ({})",
        database.table("repository", "repository_commitdata"),
        phab_db::placeholders(ids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids)
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let mut data = HashMap::new();
    for row in &found {
        data.insert(
            rows::int(row, "commitID")?,
            (
                rows::opt_text(row, "authorName")?,
                rows::opt_text(row, "commitMessage")?.unwrap_or_default(),
                rows::json(row, "commitDetails").unwrap_or(Value::Null),
            ),
        );
    }
    Ok(data)
}

/// Which commits to load.
pub(crate) enum Identifiers<'a> {
    /// Full hashes.
    Exact(&'a [String]),
    /// A hash prefix, which may match several commits.
    Prefix(&'a str),
}

/// The commits of `repository_id` with these identifiers, with their data
/// and identities, by hash.
pub(crate) async fn load(
    database: &Database,
    repository_id: i64,
    identifiers: Identifiers<'_>,
) -> Result<Vec<Commit>, PageError> {
    let table = database.table("repository", "repository_commit");
    let columns = "id, phid, commitIdentifier, epoch, summary, auditStatus, importStatus, \
         authorIdentityPHID, committerIdentityPHID";
    let found = match identifiers {
        Identifiers::Exact([]) => return Ok(Vec::new()),
        Identifiers::Exact(hashes) => {
            let sql = format!(
                "SELECT {columns} FROM {table} WHERE repositoryID = ? AND commitIdentifier IN ({})",
                phab_db::placeholders(hashes.len())
            );
            phab_db::bind_all(phab_db::sqlx::query(&sql).bind(repository_id), hashes)
                .fetch_all(database.pool())
                .await
        }
        Identifiers::Prefix(prefix) => {
            let sql = format!(
                "SELECT {columns} FROM {table} WHERE repositoryID = ? AND \
                 commitIdentifier LIKE ? ORDER BY id DESC LIMIT 100"
            );
            phab_db::sqlx::query(&sql)
                .bind(repository_id)
                .bind(format!("{prefix}%"))
                .fetch_all(database.pool())
                .await
        }
    }
    .map_err(phab_db::DbError::from)?;
    let ids: Vec<i64> = found
        .iter()
        .map(|row| rows::int(row, "id"))
        .collect::<Result<_, _>>()?;
    let mut data = load_data(database, &ids).await?;
    let mut identity_phids = Vec::new();
    for row in &found {
        for column in ["authorIdentityPHID", "committerIdentityPHID"] {
            if let Some(phid) = rows::opt_text(row, column)?.filter(|phid| !phid.is_empty()) {
                identity_phids.push(phid);
            }
        }
    }
    identity_phids.sort();
    identity_phids.dedup();
    let identities = load_identities(database, &identity_phids).await?;
    let identity = |phid: Option<String>| {
        let phid = phid?;
        let identity = identities.get(&phid)?.clone();
        Some((Phid::new(phid), identity))
    };
    let mut commits = Vec::new();
    for row in &found {
        let id = rows::int(row, "id")?;
        commits.push(Commit {
            id,
            phid: Phid::new(rows::text(row, "phid")?),
            identifier: rows::text(row, "commitIdentifier")?,
            epoch: rows::int(row, "epoch")?,
            summary: rows::text(row, "summary")?,
            audit_status: rows::text(row, "auditStatus")?,
            import_status: rows::int(row, "importStatus")?,
            author_identity: identity(rows::opt_text(row, "authorIdentityPHID")?),
            committer_identity: identity(rows::opt_text(row, "committerIdentityPHID")?),
            data: data.remove(&id),
        });
    }
    Ok(commits)
}

/// `DiffusionCommitAuditStatus`: the name, icon and color of a status.
pub(crate) fn audit_status(status: &str) -> (&'static str, &'static str, &'static str) {
    match status {
        "needs-audit" => ("Audit Required", "fa-exclamation-circle", "orange"),
        "concern-raised" => ("Concern Raised", "fa-times-circle", "red"),
        "partially-audited" => ("Partially Audited", "fa-check-circle-o", "yellow"),
        "audited" => ("Audited", "fa-check-circle", "green"),
        "needs-verification" => ("Needs Verification", "fa-refresh", "indigo"),
        _ => ("No Audits", "fa-check", "bluegrey"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_like_php() {
        assert_eq!(
            render_name("alice <alice@example.com>"),
            "<span title=\"alice@example.com\">alice</span>"
        );
        assert_eq!(render_name("alice"), "alice");
        assert_eq!(render_name("<a@b>"), "&lt;a@b&gt;");
    }
}
