//! Rows of `phabricator_differential` and `phabricator_repository`.

use std::collections::HashMap;

use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::{LoaderError, SubjectResolver};
use phab_policy::{AutomaticCapability, Capability, ExtendedPolicy, ObjectKind, PolicySubject};
use serde_json::Value;

/// `DiffusionPushCapability`: pushing to a repository.
pub const PUSH: Capability = Capability("diffusion.push");
const RESIGNED: &str = "resigned";

#[derive(Debug, Clone, PartialEq, Eq)]
/// A `differential_reviewer` row.
pub struct Reviewer {
    /// The reviewer's PHID.
    pub phid: Phid,
    /// `reviewerStatus`, e.g. `accepted` or `resigned`.
    pub status: String,
    /// `voidedPHID`: set when "Request Review" voided an accept.
    pub voided_phid: Option<Phid>,
    /// `lastActionDiffPHID`: the diff the reviewer last acted on.
    pub last_action_diff_phid: Option<Phid>,
}

impl Reviewer {
    /// A reviewer with a status and nothing else, as tests build them.
    pub fn new(phid: Phid, status: &str) -> Self {
        Self {
            phid,
            status: status.to_owned(),
            voided_phid: None,
            last_action_diff_phid: None,
        }
    }

    /// `isCurrentAction()`: the reviewer acted on `diff`, or either is
    /// unknown.
    fn is_current_action(&self, diff: Option<&Phid>) -> bool {
        match (diff, &self.last_action_diff_phid) {
            (Some(diff), Some(action)) => diff == action,
            _ => true,
        }
    }

    /// `DifferentialReviewer::isAccepted()`: an accept which was not voided,
    /// of `diff` or, with `differential.sticky-accept`, of an older one.
    pub fn is_accepted(&self, diff: Option<&Phid>, sticky_accept: bool) -> bool {
        self.status == "accepted"
            && self.voided_phid.is_none()
            && (self.is_current_action(diff) || sticky_accept)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A `differential_revision` row with its reviewers.
pub struct Revision {
    /// The revision's ID, as in `D123`.
    pub id: i64,
    /// The revision's PHID.
    pub phid: Phid,
    /// The title.
    pub title: String,
    /// The status key, like `needs-review` or `published`.
    pub status: String,
    /// The author's PHID.
    pub author_phid: Option<Phid>,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The repository's PHID, if any.
    pub repository_phid: Option<Phid>,
    /// The diff the revision shows.
    pub active_diff_phid: Option<Phid>,
    /// When it last changed.
    pub date_modified: i64,
    /// The reviewers, resigned ones included.
    pub reviewers: Vec<Reviewer>,
}

impl Revision {
    /// `DifferentialRevisionStatus::isClosedStatus()`: published and
    /// abandoned revisions are closed.
    pub fn is_closed(&self) -> bool {
        matches!(self.status.as_str(), "published" | "abandoned")
    }

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

const REVISION_COLUMNS: &str = "id, phid, title, status, authorPHID, viewPolicy, editPolicy, repositoryPHID, \
     activeDiffPHID, dateModified";

/// The revisions with these PHIDs, in no particular order; unknown
/// PHIDs are skipped.
pub async fn load_revisions(database: &Database, phids: &[Phid]) -> Result<Vec<Revision>, DbError> {
    let rows = select_by_phid(
        database,
        REVISION_COLUMNS,
        &database.table("differential", "differential_revision"),
        "phid",
        phids,
    )
    .await?;
    with_reviewers(database, &rows).await
}

/// The revisions with these IDs, in no particular order; unknown IDs are
/// skipped.
pub async fn load_revisions_by_id(
    database: &Database,
    ids: &[i64],
) -> Result<Vec<Revision>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {REVISION_COLUMNS} FROM {} WHERE id IN ({})",
        database.table("differential", "differential_revision"),
        phab_db::placeholders(ids.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
        .fetch_all(database.pool())
        .await?;
    with_reviewers(database, &rows).await
}

/// `DifferentialRevisionStatus`: the statuses which are not closed.
pub const OPEN_STATUSES: &[&str] = &[
    "needs-review",
    "needs-revision",
    "changes-planned",
    "accepted",
    "draft",
];

/// The open revisions `reviewer` reviews (`withReviewers()` and
/// `withIsOpen(true)`), newest first.
pub async fn load_open_revisions_reviewed_by(
    database: &Database,
    reviewer: &Phid,
) -> Result<Vec<Revision>, DbError> {
    let sql = format!(
        "SELECT {REVISION_COLUMNS} FROM {} WHERE status IN ({}) AND phid IN \
           (SELECT revisionPHID FROM {} WHERE reviewerPHID = ?) ORDER BY id DESC",
        database.table("differential", "differential_revision"),
        phab_db::placeholders(OPEN_STATUSES.len()),
        database.table("differential", "differential_reviewer"),
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), OPEN_STATUSES.iter().copied())
        .bind(reviewer.as_str())
        .fetch_all(database.pool())
        .await?;
    with_reviewers(database, &rows).await
}

/// The open revisions `responsible` users and projects author or review
/// (`withResponsibleUsers()`, as a union like PHP), most recently updated
/// first.
pub async fn load_responsible_revisions(
    database: &Database,
    responsible: &[Phid],
) -> Result<Vec<Revision>, DbError> {
    if responsible.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = phab_db::placeholders(responsible.len());
    let sql = format!(
        "SELECT {REVISION_COLUMNS} FROM {revisions} WHERE status IN ({statuses}) AND \
           (authorPHID IN ({placeholders}) OR phid IN \
             (SELECT revisionPHID FROM {reviewers} WHERE reviewerPHID IN ({placeholders}))) \
         ORDER BY dateModified DESC, id DESC",
        revisions = database.table("differential", "differential_revision"),
        reviewers = database.table("differential", "differential_reviewer"),
        statuses = phab_db::placeholders(OPEN_STATUSES.len()),
    );
    let query = phab_db::bind_all(phab_db::sqlx::query(&sql), OPEN_STATUSES.iter().copied());
    let query = phab_db::bind_all(query, responsible.iter().map(Phid::as_str));
    let rows = phab_db::bind_all(query, responsible.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    with_reviewers(database, &rows).await
}

/// Builds revisions from their rows, with their reviewers.
async fn with_reviewers(
    database: &Database,
    revision_rows: &[MySqlRow],
) -> Result<Vec<Revision>, DbError> {
    let phids = revision_rows
        .iter()
        .map(|row| rows::text(row, "phid").map(Phid::new))
        .collect::<Result<Vec<_>, _>>()?;
    let reviewer_rows = select_by_phid(
        database,
        "revisionPHID, reviewerPHID, reviewerStatus, voidedPHID, lastActionDiffPHID",
        &database.table("differential", "differential_reviewer"),
        "revisionPHID",
        &phids,
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
                voided_phid: rows::opt_phid(&row, "voidedPHID")?,
                last_action_diff_phid: rows::opt_phid(&row, "lastActionDiffPHID")?,
            });
    }
    revision_rows
        .iter()
        .map(|row| {
            let phid = rows::text(row, "phid")?;
            Ok(Revision {
                id: rows::int(row, "id")?,
                title: rows::text(row, "title")?,
                status: rows::text(row, "status")?,
                author_phid: rows::opt_phid(row, "authorPHID")?,
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                repository_phid: rows::opt_phid(row, "repositoryPHID")?,
                active_diff_phid: rows::opt_phid(row, "activeDiffPHID")?,
                date_modified: rows::int(row, "dateModified")?,
                reviewers: reviewers.remove(&phid).unwrap_or_default(),
                phid: Phid::new(phid),
            })
        })
        .collect()
}

/// Why [`visible_revisions()`] failed.
#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    /// Loading the policy facts failed.
    #[error(transparent)]
    Loader(#[from] phab_policy::loader::LoaderError),
    /// The policy filter refused the check.
    #[error(transparent)]
    Policy(#[from] phab_policy::PolicyError),
}

/// The revisions `viewer` can see, as `DifferentialRevisionQuery` filters
/// them.
pub async fn visible_revisions(
    database: &Database,
    config: &phab_core::Config,
    viewer: &phab_policy::Viewer,
    revisions: Vec<Revision>,
    now: i64,
) -> Result<Vec<Revision>, VisibilityError> {
    let subjects: Vec<PolicySubject> = revisions.iter().map(Revision::subject).collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
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
    Ok(revisions
        .into_iter()
        .zip(passed)
        .filter_map(|(revision, visible)| visible.then_some(revision))
        .collect())
}

/// Loads repositories for the extended policy of revisions.
pub struct Repositories<'a> {
    /// The database to read from.
    pub database: &'a Database,
}

impl SubjectResolver for Repositories<'_> {
    async fn resolve(&self, phids: &[Phid]) -> Result<HashMap<Phid, PolicySubject>, LoaderError> {
        let repositories: Vec<Phid> = phids
            .iter()
            .filter(|phid| phid.phid_type() == phab_core::PhidType::REPOSITORY)
            .cloned()
            .collect();
        Ok(crate::repository::load(self.database, &repositories)
            .await?
            .into_iter()
            .map(|repository| (repository.phid.clone(), repository.subject().clone()))
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq)]
/// A `differential_transaction` row.
pub struct Transaction {
    /// The transaction's PHID.
    pub phid: Phid,
    /// The author's PHID.
    pub author_phid: Phid,
    /// The PHID of the revision it applies to.
    pub object_phid: Phid,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The PHID of its comment, if any.
    pub comment_phid: Option<Phid>,
    /// `transactionType`, e.g. `core:subscribers`.
    pub transaction_type: String,
    /// The decoded `oldValue`.
    pub old_value: Value,
    /// The decoded `newValue`.
    pub new_value: Value,
    /// The decoded `metadata`.
    pub metadata: Value,
}

/// A transaction or a comment as a policy object: its author can always
/// see and edit it.
fn authored_subject(phid: &Phid, view: &str, edit: &str, author: &Phid) -> PolicySubject {
    let mut subject = PolicySubject::new(Some(phid.clone()), ObjectKind::Other)
        .with_policy(Capability::VIEW, view)
        .with_policy(Capability::EDIT, edit);
    subject.automatic.push(AutomaticCapability {
        principal: author.clone(),
        capabilities: None,
    });
    subject
}

impl Transaction {
    /// `PhabricatorApplicationTransaction` as a policy object.
    pub fn subject(&self) -> PolicySubject {
        authored_subject(
            &self.phid,
            &self.view_policy,
            &self.edit_policy,
            &self.author_phid,
        )
    }

    /// `PhabricatorApplicationTransaction::getMetadataValue()`.
    pub fn metadata_value(&self, key: &str) -> Option<&Value> {
        self.metadata.get(key)
    }
}

/// The transactions with these PHIDs, in no particular order.
pub async fn load_transactions(
    database: &Database,
    phids: &[Phid],
) -> Result<Vec<Transaction>, DbError> {
    let rows = select_by_phid(
        database,
        TRANSACTION_COLUMNS,
        &database.table("differential", "differential_transaction"),
        "phid",
        phids,
    )
    .await?;
    rows.iter().map(transaction_from_row).collect()
}

const TRANSACTION_COLUMNS: &str = "phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
     transactionType, oldValue, newValue, metadata, dateCreated";

fn transaction_from_row(row: &MySqlRow) -> Result<Transaction, DbError> {
    Ok(Transaction {
        phid: Phid::new(rows::text(row, "phid")?),
        author_phid: Phid::new(rows::text(row, "authorPHID")?),
        object_phid: Phid::new(rows::text(row, "objectPHID")?),
        view_policy: rows::text(row, "viewPolicy")?,
        edit_policy: rows::text(row, "editPolicy")?,
        comment_phid: rows::opt_phid(row, "commentPHID")?,
        transaction_type: rows::text(row, "transactionType")?,
        old_value: rows::json(row, "oldValue")?,
        new_value: rows::json(row, "newValue")?,
        metadata: rows::json(row, "metadata")?,
    })
}

/// The transactions of a revision, oldest first, with when they happened:
/// the timeline of the revision page.
pub async fn load_object_transactions(
    database: &Database,
    object: &Phid,
) -> Result<Vec<(Transaction, i64)>, DbError> {
    let sql = format!(
        "SELECT {TRANSACTION_COLUMNS} FROM {} WHERE objectPHID = ? ORDER BY id",
        database.table("differential", "differential_transaction")
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(object.as_str())
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok((transaction_from_row(row)?, rows::int(row, "dateCreated")?)))
        .collect()
}

/// A published inline comment (`DifferentialTransactionComment` with a
/// changeset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComment {
    /// The comment's PHID.
    pub phid: Phid,
    /// Its author.
    pub author_phid: Phid,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The text, in Remarkup.
    pub content: String,
    /// The changeset it is on.
    pub changeset_id: i64,
    /// On the new side of the diff, or the old one.
    pub is_new_file: bool,
    /// The first line.
    pub line_number: i64,
    /// How many lines after the first it spans.
    pub line_length: i64,
    /// When it was written.
    pub date_created: i64,
}

impl InlineComment {
    /// As a policy object, like a comment.
    pub fn subject(&self) -> PolicySubject {
        authored_subject(
            &self.phid,
            &self.view_policy,
            &self.edit_policy,
            &self.author_phid,
        )
    }
}

/// The published, not deleted inline comments on these changesets, in the
/// order they were written. Drafts have no transaction yet.
pub async fn load_inline_comments(
    database: &Database,
    changesets: &[i64],
) -> Result<Vec<InlineComment>, DbError> {
    if changesets.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT phid, authorPHID, viewPolicy, editPolicy, content, changesetID, isNewFile, lineNumber, \
           lineLength, dateCreated FROM {} \
         WHERE changesetID IN ({}) AND transactionPHID IS NOT NULL AND isDeleted = 0 ORDER BY id",
        database.table("differential", "differential_transaction_comment"),
        phab_db::placeholders(changesets.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), changesets.iter().copied())
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| {
            Ok(InlineComment {
                phid: Phid::new(rows::text(row, "phid")?),
                author_phid: Phid::new(rows::text(row, "authorPHID")?),
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                content: rows::text(row, "content")?,
                changeset_id: rows::int(row, "changesetID")?,
                is_new_file: rows::flag(row, "isNewFile")?,
                line_number: rows::int(row, "lineNumber")?,
                line_length: rows::int(row, "lineLength")?,
                date_created: rows::int(row, "dateCreated")?,
            })
        })
        .collect()
}

/// Which of `subjects` `viewer` can see, in order.
pub async fn visible_subjects(
    database: &Database,
    config: &phab_core::Config,
    viewer: &phab_policy::Viewer,
    subjects: &[PolicySubject],
    now: i64,
) -> Result<Vec<bool>, VisibilityError> {
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts = phab_policy::loader::load_facts(
        database,
        config,
        viewer,
        &refs,
        &Repositories { database },
        now,
    )
    .await?;
    Ok(phab_policy::filter(
        viewer,
        &refs,
        &[Capability::VIEW],
        &facts,
    )?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A `differential_transaction_comment` row.
pub struct Comment {
    /// The comment's PHID.
    pub phid: Phid,
    /// The author's PHID.
    pub author_phid: Phid,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The text, empty when `NULL`.
    pub content: String,
}

impl Comment {
    /// `PhabricatorApplicationTransactionComment` as a policy object.
    pub fn subject(&self) -> PolicySubject {
        authored_subject(
            &self.phid,
            &self.view_policy,
            &self.edit_policy,
            &self.author_phid,
        )
    }

    /// `PhabricatorApplicationTransactionComment::isEmptyComment()`.
    pub fn is_empty(&self) -> bool {
        self.content
            .trim_matches([' ', '\t', '\n', '\r', '\0', '\u{0B}'])
            .is_empty()
    }
}

/// The comments with these PHIDs, in no particular order.
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
                author_phid: Phid::new(rows::text(row, "authorPHID")?),
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                content: rows::opt_text(row, "content")?.unwrap_or_default(),
            })
        })
        .collect()
}
