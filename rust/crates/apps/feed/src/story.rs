//! Feed stories: the rows `PhabricatorFeedQuery` loads and the filtering of
//! `PhabricatorFeedStory::loadAllFromRows()` and `isVisibleInFeed()`.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
};

use phab_app_differential::{
    Comment, Repositories, Revision, Transaction,
    feed::{FeedVisibility, feed_visibility, story_visibility},
    storage::{load_comments, load_revisions, load_transactions},
};
use phab_conduit::MethodError;
use phab_core::{Config, Phid, PhidType, config::is_truthy};
use phab_db::{
    Database, DbError, bind_all, placeholders, rows,
    sqlx::{Row, mysql::MySqlRow},
};
use phab_policy::{Capability, PolicySubject, Viewer, filter, loader::load_facts};
use serde_json::Value;

/// The only `PhabricatorFeedStory` subclass. Rows of other types are dropped,
/// as PHP drops rows whose class does not exist.
pub const TRANSACTION_STORY: &str = "PhabricatorApplicationTransactionFeedStory";

/// A `feed_storydata` row.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryRow {
    pub id: u64,
    pub phid: String,
    pub chronological_key: u64,
    pub story_type: String,
    pub story_data: Value,
    pub author_phid: String,
}

impl StoryRow {
    /// `PhabricatorFeedStoryData::getEpoch()`.
    pub fn epoch(&self) -> u64 {
        self.chronological_key >> 32
    }
}

/// What a page of `PhabricatorFeedIDQuery` selects.
#[derive(Debug, Clone, Copy, Default)]
pub struct Page<'a> {
    /// `withIDs()`.
    pub ids: Option<&'a [u64]>,
    /// Only rows after this id, in the order of the query.
    pub after: Option<u64>,
    /// Newest first instead of oldest first.
    pub reversed: bool,
    /// `None` loads every row.
    pub limit: Option<usize>,
}

fn column_error(column: &str) -> DbError {
    DbError::ColumnType {
        column: column.to_owned(),
    }
}

fn unsigned(row: &MySqlRow, column: &str) -> Result<u64, DbError> {
    if let Ok(value) = row.try_get::<u64, _>(column) {
        return Ok(value);
    }
    u64::try_from(rows::int(row, column)?).map_err(|_| column_error(column))
}

/// `PhabricatorFeedQuery::loadPage()`: stories without references are
/// notifications and are not part of the feed.
pub async fn load_page(database: &Database, page: Page<'_>) -> Result<Vec<StoryRow>, DbError> {
    let mut conditions = Vec::new();
    if let Some(ids) = page.ids {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        conditions.push(format!("story.id IN ({})", placeholders(ids.len())));
    }
    if page.after.is_some() {
        conditions.push(
            if page.reversed {
                "story.id < ?"
            } else {
                "story.id > ?"
            }
            .to_owned(),
        );
    }
    let mut sql = format!(
        "SELECT story.id, story.phid, story.chronologicalKey, story.storyType, story.storyData, \
         story.authorPHID FROM {} story JOIN {} ref ON ref.chronologicalKey = story.chronologicalKey",
        database.table("feed", "feed_storydata"),
        database.table("feed", "feed_storyreference"),
    );
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" GROUP BY story.chronologicalKey ORDER BY story.id ");
    sql.push_str(if page.reversed { "DESC" } else { "ASC" });
    if let Some(limit) = page.limit {
        let _ = write!(sql, " LIMIT {limit}");
    }
    let query = bind_all(
        phab_db::sqlx::query(&sql),
        page.ids.unwrap_or_default().iter().copied(),
    );
    let query = match page.after {
        Some(after) => query.bind(after),
        None => query,
    };
    let rows = query.fetch_all(database.pool()).await?;
    rows.iter()
        .map(|row| {
            Ok(StoryRow {
                id: unsigned(row, "id")?,
                phid: rows::text(row, "phid")?,
                chronological_key: unsigned(row, "chronologicalKey")?,
                story_type: rows::text(row, "storyType")?,
                story_data: rows::json(row, "storyData")?,
                author_phid: rows::text(row, "authorPHID")?,
            })
        })
        .collect()
}

/// A story of the page with the objects it needs.
struct Candidate {
    row: StoryRow,
    object: Phid,
    transactions: Vec<Phid>,
}

fn is_revision_transaction(phid: &Phid) -> bool {
    phid.phid_type() == PhidType::TRANSACTION && phid.subtype() == Some("DREV")
}

/// Reads `transactionPHIDs` and `objectPHID`; `None` if the story is hidden
/// whatever the viewer.
fn candidate(row: StoryRow) -> Result<Option<Candidate>, MethodError> {
    let Value::Object(data) = &row.story_data else {
        // PHP fails reading the values of anything but a dictionary.
        return Err(MethodError::Unsupported(
            "feed story data is not a dictionary",
        ));
    };
    let transactions: Vec<Phid> = match data.get("transactionPHIDs") {
        Some(Value::Array(items)) => items.iter().collect::<Vec<_>>(),
        Some(Value::Object(entries)) => entries.values().collect(),
        _ => return Err(MethodError::Unsupported("feed story without transactions")),
    }
    .into_iter()
    .map(|phid| phid.as_str().map(Phid::new))
    .collect::<Option<_>>()
    .ok_or(MethodError::Unsupported(
        "feed story with invalid transactions",
    ))?;
    // isVisibleInFeed() is false without transactions.
    if transactions.is_empty() {
        return Ok(None);
    }
    let object = match data.get("objectPHID") {
        Some(Value::String(phid)) if is_truthy(&Value::from(phid.as_str())) => {
            Phid::new(phid.as_str())
        }
        Some(value) if is_truthy(value) => {
            return Err(MethodError::Unsupported(
                "feed story with an invalid object",
            ));
        }
        // renderView() fails without the object's handle, and the method
        // skips stories which fail to render.
        _ => return Err(MethodError::Unsupported("feed story without an object")),
    };
    let supported = object.phid_type() == PhidType::REVISION
        && transactions.iter().all(is_revision_transaction);
    if !supported {
        return Err(MethodError::Unsupported(
            "feed story about objects other than revisions",
        ));
    }
    Ok(Some(Candidate {
        row,
        object,
        transactions,
    }))
}

fn sorted(mut phids: Vec<Phid>) -> Vec<Phid> {
    phids.sort();
    phids.dedup();
    phids
}

/// The objects of a page's stories the viewer can see.
struct VisibleObjects {
    revisions: HashSet<Phid>,
    /// Transactions whose revision is visible too.
    transactions: HashMap<Phid, Transaction>,
    comments: HashMap<Phid, Comment>,
}

async fn load_visible_objects(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    candidates: &[Candidate],
    now: i64,
) -> Result<VisibleObjects, MethodError> {
    let transaction_phids = sorted(
        candidates
            .iter()
            .flat_map(|candidate| candidate.transactions.iter().cloned())
            .collect(),
    );
    let transactions = load_transactions(database, &transaction_phids)
        .await
        .map_err(MethodError::internal)?;
    let revision_phids = sorted(
        candidates
            .iter()
            .map(|candidate| candidate.object.clone())
            .chain(
                transactions
                    .iter()
                    .map(|xaction| xaction.object_phid.clone()),
            )
            .collect(),
    );
    let revisions = load_revisions(database, &revision_phids)
        .await
        .map_err(MethodError::internal)?;
    let comment_phids = sorted(
        transactions
            .iter()
            .filter_map(|xaction| xaction.comment_phid.clone())
            .collect(),
    );
    let comments = load_comments(database, &comment_phids)
        .await
        .map_err(MethodError::internal)?;

    let subjects: Vec<PolicySubject> = revisions
        .iter()
        .map(Revision::subject)
        .chain(transactions.iter().map(Transaction::subject))
        .chain(comments.iter().map(Comment::subject))
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts = load_facts(
        database,
        config,
        viewer,
        &refs,
        &Repositories { database },
        now,
    )
    .await
    .map_err(MethodError::internal)?;
    let passed =
        filter(viewer, &refs, &[Capability::VIEW], &facts).map_err(MethodError::internal)?;
    let can_see: HashSet<Phid> = refs
        .iter()
        .zip(passed)
        .filter(|(_, ok)| *ok)
        .filter_map(|(subject, _)| subject.phid.clone())
        .collect();

    let revisions: HashSet<Phid> = revisions
        .into_iter()
        .map(|revision| revision.phid)
        .filter(|phid| can_see.contains(phid))
        .collect();
    // The transaction query drops transactions whose object is not visible,
    // and attaches the comments the viewer can see.
    let transactions = transactions
        .into_iter()
        .filter(|xaction| {
            can_see.contains(&xaction.phid) && revisions.contains(&xaction.object_phid)
        })
        .map(|xaction| (xaction.phid.clone(), xaction))
        .collect();
    let comments = comments
        .into_iter()
        .filter(|comment| can_see.contains(&comment.phid))
        .map(|comment| (comment.phid.clone(), comment))
        .collect();
    Ok(VisibleObjects {
        revisions,
        transactions,
        comments,
    })
}

/// The stories of a page `viewer` can see, in order: `willFilterPage()` of
/// `PhabricatorFeedQuery`, then the policy filter.
pub async fn visible_stories(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    rows: Vec<StoryRow>,
    now: i64,
) -> Result<Vec<StoryRow>, MethodError> {
    let mut candidates = Vec::new();
    for row in rows {
        if !row.story_type.eq_ignore_ascii_case(TRANSACTION_STORY) {
            continue;
        }
        if let Some(candidate) = candidate(row)? {
            candidates.push(candidate);
        }
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let objects = load_visible_objects(database, config, viewer, &candidates, now).await?;

    // PhabricatorFeedStory::getPolicy() is the most open policy.
    let can_see_stories = viewer.is_omnipotent || viewer.is_logged_in() || config.allow_public();

    let mut visible = Vec::new();
    for candidate in candidates {
        // Every object the story is about must be visible.
        if !objects.revisions.contains(&candidate.object) {
            continue;
        }
        let Some(xactions) = candidate
            .transactions
            .iter()
            .map(|phid| objects.transactions.get(phid))
            .collect::<Option<Vec<&Transaction>>>()
        else {
            continue;
        };
        let feed: Vec<FeedVisibility> = xactions
            .iter()
            .map(|xaction| {
                let comment = xaction
                    .comment_phid
                    .as_ref()
                    .and_then(|phid| objects.comments.get(phid));
                feed_visibility(xaction, comment)
            })
            .collect();
        match story_visibility(&feed) {
            FeedVisibility::Visible => {
                if can_see_stories {
                    visible.push(candidate.row);
                }
            }
            FeedVisibility::Hidden => {}
            FeedVisibility::Unknown => {
                return Err(MethodError::Unsupported(
                    "feed visibility of a transaction is not ported",
                ));
            }
        }
    }
    Ok(visible)
}
