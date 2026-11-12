//! Rows of `phabricator_differential` and `phabricator_repository`.

use std::collections::HashMap;

use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::{LoaderError, SubjectResolver};
use phab_policy::{AutomaticCapability, Capability, ExtendedPolicy, ObjectKind, PolicySubject};
use serde_json::Value;

pub const PUSH: Capability = Capability("diffusion.push");
const RESIGNED: &str = "resigned";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub phid: Phid,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub phid: Phid,
    pub author_phid: Option<Phid>,
    pub view_policy: String,
    pub edit_policy: String,
    pub repository_phid: Option<Phid>,
    pub reviewers: Vec<Reviewer>,
}

impl Revision {
    /// `DifferentialRevision` as a policy object: the author can always view
    /// and edit, viewing needs the repository too, and the author and the
    /// reviewers who haven't resigned are implicit subscribers.
    pub fn subject(&self) -> PolicySubject {
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Revision)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone());
        subject.subscribable = true;
        if let Some(author) = &self.author_phid {
            subject.automatic.push(AutomaticCapability {
                principal: author.clone(),
                capabilities: None,
            });
            subject.implicit_subscribers.push(author.clone());
        }
        if let Some(repository) = &self.repository_phid {
            subject.extended.push(ExtendedPolicy {
                for_capability: Capability::VIEW,
                object: repository.clone(),
                capabilities: vec![Capability::VIEW],
            });
        }
        subject.implicit_subscribers.extend(
            self.reviewers
                .iter()
                .filter(|reviewer| reviewer.status != RESIGNED)
                .map(|reviewer| reviewer.phid.clone()),
        );
        subject
    }
}

fn phids_of(phids: &[Phid]) -> impl Iterator<Item = &str> {
    phids.iter().map(Phid::as_str)
}

async fn select_by_phid(
    database: &Database,
    columns: &str,
    table: &str,
    key: &str,
    phids: &[Phid],
) -> Result<Vec<MySqlRow>, DbError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {columns} FROM {table} WHERE {key} IN ({})",
        phab_db::placeholders(phids.len())
    );
    Ok(
        phab_db::bind_all(phab_db::sqlx::query(&sql), phids_of(phids))
            .fetch_all(database.pool())
            .await?,
    )
}

pub async fn load_revisions(database: &Database, phids: &[Phid]) -> Result<Vec<Revision>, DbError> {
    let rows = select_by_phid(
        database,
        "phid, authorPHID, viewPolicy, editPolicy, repositoryPHID",
        &database.table("differential", "differential_revision"),
        "phid",
        phids,
    )
    .await?;
    let reviewer_rows = select_by_phid(
        database,
        "revisionPHID, reviewerPHID, reviewerStatus",
        &database.table("differential", "differential_reviewer"),
        "revisionPHID",
        phids,
    )
    .await?;
    let mut reviewers: HashMap<String, Vec<Reviewer>> = HashMap::new();
    for row in reviewer_rows {
        reviewers
            .entry(rows::text(&row, "revisionPHID")?)
            .or_default()
            .push(Reviewer {
                phid: Phid::new(rows::text(&row, "reviewerPHID")?),
                status: rows::text(&row, "reviewerStatus")?,
            });
    }
    rows.iter()
        .map(|row| {
            let phid = rows::text(row, "phid")?;
            Ok(Revision {
                author_phid: rows::opt_text(row, "authorPHID")?
                    .filter(|author| !author.is_empty())
                    .map(Phid::new),
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                repository_phid: rows::opt_text(row, "repositoryPHID")?
                    .filter(|repository| !repository.is_empty())
                    .map(Phid::new),
                reviewers: reviewers.remove(&phid).unwrap_or_default(),
                phid: Phid::new(phid),
            })
        })
        .collect()
}

/// Loads repositories for the extended policy of revisions.
pub struct Repositories<'a> {
    pub database: &'a Database,
}

impl SubjectResolver for Repositories<'_> {
    async fn resolve(&self, phids: &[Phid]) -> Result<HashMap<Phid, PolicySubject>, LoaderError> {
        let repositories: Vec<Phid> = phids
            .iter()
            .filter(|phid| phid.phid_type() == phab_core::PhidType::REPOSITORY)
            .cloned()
            .collect();
        let rows = select_by_phid(
            self.database,
            "phid, viewPolicy, editPolicy, pushPolicy, spacePHID",
            &self.database.table("repository", "repository"),
            "phid",
            &repositories,
        )
        .await?;
        let mut subjects = HashMap::new();
        for row in rows {
            let phid = Phid::new(rows::text(&row, "phid")?);
            let mut subject = PolicySubject::new(Some(phid.clone()), ObjectKind::Other)
                .with_policy(Capability::VIEW, rows::text(&row, "viewPolicy")?)
                .with_policy(Capability::EDIT, rows::text(&row, "editPolicy")?)
                .with_policy(PUSH, rows::text(&row, "pushPolicy")?);
            subject.capabilities.push(PUSH);
            subject.space = Some(rows::opt_text(&row, "spacePHID")?.map(Phid::new));
            subjects.insert(phid, subject);
        }
        Ok(subjects)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transaction {
    pub phid: Phid,
    pub author_phid: String,
    pub object_phid: Phid,
    pub view_policy: String,
    pub edit_policy: String,
    pub comment_phid: Option<Phid>,
    pub transaction_type: String,
    pub old_value: Value,
    pub new_value: Value,
    pub metadata: Value,
}

impl Transaction {
    /// `PhabricatorApplicationTransaction` as a policy object.
    pub fn subject(&self) -> PolicySubject {
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone());
        subject.automatic.push(AutomaticCapability {
            principal: Phid::new(self.author_phid.as_str()),
            capabilities: None,
        });
        subject
    }

    pub fn metadata_value(&self, key: &str) -> Option<&Value> {
        self.metadata.get(key)
    }
}

pub async fn load_transactions(
    database: &Database,
    phids: &[Phid],
) -> Result<Vec<Transaction>, DbError> {
    let rows = select_by_phid(
        database,
        "phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, transactionType, \
         oldValue, newValue, metadata",
        &database.table("differential", "differential_transaction"),
        "phid",
        phids,
    )
    .await?;
    rows.iter()
        .map(|row| {
            Ok(Transaction {
                phid: Phid::new(rows::text(row, "phid")?),
                author_phid: rows::text(row, "authorPHID")?,
                object_phid: Phid::new(rows::text(row, "objectPHID")?),
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                comment_phid: rows::opt_text(row, "commentPHID")?
                    .filter(|phid| !phid.is_empty())
                    .map(Phid::new),
                transaction_type: rows::text(row, "transactionType")?,
                old_value: rows::json(row, "oldValue")?,
                new_value: rows::json(row, "newValue")?,
                metadata: rows::json(row, "metadata")?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub phid: Phid,
    pub author_phid: String,
    pub view_policy: String,
    pub edit_policy: String,
    pub content: String,
}

impl Comment {
    pub fn subject(&self) -> PolicySubject {
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone());
        subject.automatic.push(AutomaticCapability {
            principal: Phid::new(self.author_phid.as_str()),
            capabilities: None,
        });
        subject
    }

    /// `PhabricatorApplicationTransactionComment::isEmptyComment()`.
    pub fn is_empty(&self) -> bool {
        self.content
            .trim_matches([' ', '\t', '\n', '\r', '\0', '\u{0B}'])
            .is_empty()
    }
}

pub async fn load_comments(database: &Database, phids: &[Phid]) -> Result<Vec<Comment>, DbError> {
    let rows = select_by_phid(
        database,
        "phid, authorPHID, viewPolicy, editPolicy, content",
        &database.table("differential", "differential_transaction_comment"),
        "phid",
        phids,
    )
    .await?;
    rows.iter()
        .map(|row| {
            Ok(Comment {
                phid: Phid::new(rows::text(row, "phid")?),
                author_phid: rows::text(row, "authorPHID")?,
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                content: rows::opt_text(row, "content")?.unwrap_or_default(),
            })
        })
        .collect()
}
