//! Feed stories: the rows `PhabricatorFeedQuery` loads and the filtering of
//! `PhabricatorFeedStory::loadAllFromRows()` and `isVisibleInFeed()`.

use std::collections::{HashMap, HashSet};

use phab_app_differential::feed::FeedVisibility;

use phab_app_differential::{Comment, Repositories, Revision, Transaction, feed, storage};
use phab_conduit::MethodError;
use phab_core::{Config, Phid, PhidType, php};
use phab_db::{Database, DbError, rows};
use phab_policy::loader::LoaderError;
use phab_policy::{Capability, PolicyError, PolicySubject, Viewer, loader};
use serde_json::Value;
use thiserror::Error;

/// The only `PhabricatorFeedStory` subclass. Rows of other types are dropped,
/// as PHP drops rows whose class does not exist.
pub const TRANSACTION_STORY: &str = "PhabricatorApplicationTransactionFeedStory";

#[derive(Debug, Error)]
/// Why stories could not be loaded.
pub enum StoryError {
    /// The answer depends on something not ported; PHP must answer.
    #[error("{0}")]
    Unsupported(&'static str),
    /// Reading the database failed.
    #[error(transparent)]
    Database(#[from] DbError),
    /// Loading the objects to check failed.
    #[error(transparent)]
    Loader(#[from] LoaderError),
    /// Applying the policies failed.
    #[error(transparent)]
    Policy(#[from] PolicyError),
}

impl From<StoryError> for MethodError {
    fn from(error: StoryError) -> Self {
        match error {
            StoryError::Unsupported(reason) => Self::Unsupported(reason),
            other => Self::internal(other),
        }
    }
}

/// A `feed_storydata` row.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryRow {
    /// The story's id.
    pub id: u64,
    /// The story's PHID.
    pub phid: String,
    /// The epoch in the high 32 bits, a counter in the low ones.
    pub chronological_key: u64,
    /// The `PhabricatorFeedStory` subclass.
    pub story_type: String,
    /// The decoded `storyData`.
    pub story_data: Value,
    /// The author's PHID.
    pub author_phid: String,
    /// `dateCreated`.
    pub date_created: i64,
}

impl StoryRow {
    /// `PhabricatorFeedStoryData::getEpoch()`.
    pub fn epoch(&self) -> u64 {
        self.chronological_key >> 32
    }
}

/// The order of `PhabricatorFeedIDQuery`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Order {
    /// `setOrder('oldest')`: by increasing id.
    #[default]
    Oldest,
    /// The reversed order, used to page backwards.
    Newest,
}

/// What pages are ordered and cut by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Column {
    /// The story id, as `PhabricatorFeedIDQuery` pages.
    #[default]
    Id,
    /// The chronological key, as `PhabricatorFeedQuery` pages `/feed/`.
    ChronologicalKey,
}

impl Column {
    fn sql(self) -> &'static str {
        match self {
            Self::Id => "story.id",
            Self::ChronologicalKey => "story.chronologicalKey",
        }
    }
}

/// What a page of `PhabricatorFeedIDQuery` selects.
#[derive(Debug, Clone, Copy, Default)]
pub struct Page<'a> {
    /// `withIDs()`.
    pub ids: Option<&'a [u64]>,
    /// `PhabricatorFeedQuery::withChronologicalKeys()`.
    pub keys: Option<&'a [u64]>,
    /// Only rows past this value of `column`, in `order`.
    pub after: Option<u64>,
    /// The order of the rows and of `after`.
    pub order: Order,
    /// `None` loads every row.
    pub limit: Option<usize>,
    /// The column of `order` and `after`.
    pub column: Column,
    /// `PhabricatorFeedQuery::withFilterPHIDs()`: only stories referencing
    /// one of these objects, like their author.
    pub references: Option<&'a [Phid]>,
}

impl Page<'_> {
    fn sql(&self, database: &Database) -> String {
        let column = self.column.sql();
        let mut conditions = Vec::new();
        if let Some(ids) = self.ids {
            conditions.push(format!(
                "story.id IN ({})",
                phab_db::placeholders(ids.len())
            ));
        }
        if let Some(keys) = self.keys {
            conditions.push(format!(
                "ref.chronologicalKey IN ({})",
                phab_db::placeholders(keys.len())
            ));
        }
        if let Some(references) = self.references {
            conditions.push(format!(
                "ref.objectPHID IN ({})",
                phab_db::placeholders(references.len())
            ));
        }
        if self.after.is_some() {
            conditions.push(match self.order {
                Order::Oldest => format!("{column} > ?"),
                Order::Newest => format!("{column} < ?"),
            });
        }
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let direction = match self.order {
            Order::Oldest => "ASC",
            Order::Newest => "DESC",
        };
        let limit = self
            .limit
            .map(|limit| format!(" LIMIT {limit}"))
            .unwrap_or_default();
        format!(
            "SELECT story.id, story.phid, story.chronologicalKey, story.storyType, \
             story.storyData, story.authorPHID, story.dateCreated FROM {} story JOIN {} ref \
             ON ref.chronologicalKey = story.chronologicalKey{filter} \
             GROUP BY story.chronologicalKey ORDER BY {column} {direction}{limit}",
            database.table("feed", "feed_storydata"),
            database.table("feed", "feed_storyreference"),
        )
    }
}

/// `PhabricatorFeedQuery::loadPage()`: stories without references are
/// notifications and are not part of the feed.
pub async fn load_page(database: &Database, page: Page<'_>) -> Result<Vec<StoryRow>, DbError> {
    if page.ids.is_some_and(<[u64]>::is_empty)
        || page.keys.is_some_and(<[u64]>::is_empty)
        || page.references.is_some_and(<[Phid]>::is_empty)
    {
        return Ok(Vec::new());
    }
    let sql = page.sql(database);
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        page.ids.unwrap_or_default().iter().copied(),
    );
    let query = phab_db::bind_all(query, page.keys.unwrap_or_default().iter().copied());
    let query = phab_db::bind_all(
        query,
        page.references.unwrap_or_default().iter().map(Phid::as_str),
    );
    let rows = phab_db::bind_all(query, page.after)
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(story_row).collect()
}

fn story_row(row: &phab_db::sqlx::mysql::MySqlRow) -> Result<StoryRow, DbError> {
    Ok(StoryRow {
        id: rows::uint(row, "id")?,
        phid: rows::text(row, "phid")?,
        chronological_key: rows::uint(row, "chronologicalKey")?,
        story_type: rows::text(row, "storyType")?,
        story_data: rows::json(row, "storyData")?,
        author_phid: rows::text(row, "authorPHID")?,
        date_created: rows::int(row, "dateCreated")?,
    })
}

/// What a page of `PhabricatorNotificationQuery` selects: a user's
/// notifications, newest first by default.
#[derive(Debug, Clone, Copy)]
pub struct NotificationPage<'a> {
    /// `withUserPHIDs()`.
    pub user: &'a Phid,
    /// `withUnread(true)`: only the notifications not viewed yet.
    pub unread: bool,
    /// Only rows past this chronological key, in `order`.
    pub after: Option<u64>,
    /// The order of the rows and of `after`.
    pub order: Order,
    /// How many rows to load.
    pub limit: usize,
}

/// `PhabricatorNotificationQuery::loadPage()`: the stories of the page,
/// each with whether the user viewed it (`hasViewed`).
pub async fn load_notification_page(
    database: &Database,
    page: NotificationPage<'_>,
) -> Result<Vec<(StoryRow, bool)>, DbError> {
    let mut conditions = vec!["notification.userPHID = ?"];
    if page.unread {
        conditions.push("notification.hasViewed = 0");
    }
    let (comparison, direction) = match page.order {
        Order::Oldest => ("notification.chronologicalKey > ?", "ASC"),
        Order::Newest => ("notification.chronologicalKey < ?", "DESC"),
    };
    if page.after.is_some() {
        conditions.push(comparison);
    }
    let sql = format!(
        "SELECT story.id, story.phid, story.chronologicalKey, story.storyType, \
         story.storyData, story.authorPHID, story.dateCreated, notification.hasViewed \
         FROM {} notification JOIN {} story \
         ON notification.chronologicalKey = story.chronologicalKey WHERE {} \
         ORDER BY notification.chronologicalKey {direction} LIMIT {}",
        database.table("feed", "feed_storynotification"),
        database.table("feed", "feed_storydata"),
        conditions.join(" AND "),
        page.limit,
    );
    let query = phab_db::sqlx::query(&sql).bind(page.user.as_str());
    let rows = phab_db::bind_all(query, page.after)
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok((story_row(row)?, rows::flag(row, "hasViewed")?)))
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

impl Candidate {
    /// Reads `transactionPHIDs` and `objectPHID`; `None` if the story is
    /// hidden whatever the viewer.
    fn parse(row: StoryRow) -> Result<Option<Self>, StoryError> {
        let Value::Object(data) = &row.story_data else {
            // PHP fails reading the values of anything but a dictionary.
            return Err(StoryError::Unsupported(
                "feed story data is not a dictionary",
            ));
        };
        let transactions: Vec<Phid> = match data.get("transactionPHIDs") {
            Some(Value::Array(items)) => items.iter().collect(),
            Some(Value::Object(entries)) => entries.values().collect::<Vec<_>>(),
            _ => return Err(StoryError::Unsupported("feed story without transactions")),
        }
        .into_iter()
        .map(|phid| phid.as_str().map(Phid::new))
        .collect::<Option<_>>()
        .ok_or(StoryError::Unsupported(
            "feed story with invalid transactions",
        ))?;
        // isVisibleInFeed() is false without transactions.
        if transactions.is_empty() {
            return Ok(None);
        }
        let object = match data.get("objectPHID") {
            Some(Value::String(phid)) if php::is_truthy_str(phid) => Phid::new(phid),
            Some(value) if php::is_truthy(value) => {
                return Err(StoryError::Unsupported("feed story with an invalid object"));
            }
            // renderView() fails without the object's handle, and the method
            // skips stories which fail to render.
            _ => return Err(StoryError::Unsupported("feed story without an object")),
        };
        if object.phid_type() != PhidType::REVISION
            || !transactions.iter().all(is_revision_transaction)
        {
            return Err(StoryError::Unsupported(
                "feed story about objects other than revisions",
            ));
        }
        Ok(Some(Self {
            row,
            object,
            transactions,
        }))
    }
}

fn sorted(phids: impl IntoIterator<Item = Phid>) -> Vec<Phid> {
    let mut phids: Vec<Phid> = phids.into_iter().collect();
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

impl VisibleObjects {
    async fn load(
        database: &Database,
        config: &Config,
        viewer: &Viewer,
        candidates: &[Candidate],
        now: i64,
    ) -> Result<Self, StoryError> {
        let transactions = storage::load_transactions(
            database,
            &sorted(
                candidates
                    .iter()
                    .flat_map(|candidate| candidate.transactions.iter().cloned()),
            ),
        )
        .await?;
        let revisions = storage::load_revisions(
            database,
            &sorted(
                candidates
                    .iter()
                    .map(|candidate| candidate.object.clone())
                    .chain(
                        transactions
                            .iter()
                            .map(|xaction| xaction.object_phid.clone()),
                    ),
            ),
        )
        .await?;
        let comments = storage::load_comments(
            database,
            &sorted(
                transactions
                    .iter()
                    .filter_map(|xaction| xaction.comment_phid.clone()),
            ),
        )
        .await?;

        let subjects: Vec<PolicySubject> = revisions
            .iter()
            .map(Revision::subject)
            .chain(transactions.iter().map(Transaction::subject))
            .chain(comments.iter().map(Comment::subject))
            .collect();
        let refs: Vec<&PolicySubject> = subjects.iter().collect();
        let facts = loader::load_facts(
            database,
            config,
            viewer,
            &refs,
            &Repositories { database },
            now,
        )
        .await?;
        let passed = phab_policy::filter(viewer, &refs, &[Capability::VIEW], &facts)?;
        let can_see: HashSet<&Phid> = refs
            .iter()
            .zip(passed)
            .filter_map(|(subject, ok)| subject.phid.as_ref().filter(|_| ok))
            .collect();

        let revisions: HashSet<Phid> = revisions
            .into_iter()
            .map(|revision| revision.phid)
            .filter(|phid| can_see.contains(phid))
            .collect();
        // The transaction query drops transactions whose object is not
        // visible, and attaches the comments the viewer can see.
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
        Ok(Self {
            revisions,
            transactions,
            comments,
        })
    }

    /// The story's transactions and whether each shows in feed; `None` if
    /// the viewer can't see every object the story is about.
    fn transactions(&self, candidate: &Candidate) -> Option<Vec<(Transaction, FeedVisibility)>> {
        if !self.revisions.contains(&candidate.object) {
            return None;
        }
        candidate
            .transactions
            .iter()
            .map(|phid| {
                let xaction = self.transactions.get(phid)?;
                let comment = xaction
                    .comment_phid
                    .as_ref()
                    .and_then(|phid| self.comments.get(phid));
                Some((xaction.clone(), feed::feed_visibility(xaction, comment)))
            })
            .collect()
    }
}

/// What to do with a story whose visibility depends on code that is not
/// ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unported {
    /// Fail with [`StoryError::Unsupported`], so PHP answers.
    Fail,
    /// Leave the story out and count it.
    Skip,
}

/// A story the viewer can see, with its transactions.
#[derive(Debug, Clone)]
pub struct VisibleStory {
    /// The story row.
    pub row: StoryRow,
    /// The revision it is about.
    pub object: Phid,
    /// Its transactions, in order, and whether each is hidden from feed
    /// (`shouldHideForFeed()`).
    pub transactions: Vec<(Transaction, bool)>,
}

impl VisibleStory {
    /// A row which did not go through [`load_visible()`] yet, for queries
    /// whose loaded and filtered rows must have the same type.
    pub fn unfiltered(row: StoryRow) -> Self {
        Self {
            row,
            object: Phid::new(""),
            transactions: Vec::new(),
        }
    }

    /// `getPrimaryTransaction()`: the first transaction shown in feed.
    pub fn primary_transaction(&self) -> Option<&Transaction> {
        phab_app_differential::title::primary_transaction(&self.transactions)
    }
}

/// The stories of a page a viewer can see.
#[derive(Debug, Clone, Default)]
pub struct Stories {
    /// The visible stories, in order.
    pub visible: Vec<VisibleStory>,
    /// How many stories were left out because Rust can't tell whether the
    /// viewer may see them ([`Unported::Skip`]).
    pub skipped: usize,
}

/// The stories of a page `viewer` can see, in order: `willFilterPage()` of
/// `PhabricatorFeedQuery`, then the policy filter.
pub async fn load_visible(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    rows: Vec<StoryRow>,
    now: i64,
    unported: Unported,
) -> Result<Stories, StoryError> {
    let mut skipped = 0;
    let mut candidates = Vec::new();
    for row in rows {
        if !row.story_type.eq_ignore_ascii_case(TRANSACTION_STORY) {
            continue;
        }
        match Candidate::parse(row) {
            Ok(Some(candidate)) => candidates.push(candidate),
            Ok(None) => {}
            Err(StoryError::Unsupported(_)) if unported == Unported::Skip => skipped += 1,
            Err(error) => return Err(error),
        }
    }
    if candidates.is_empty() {
        return Ok(Stories {
            visible: Vec::new(),
            skipped,
        });
    }
    let objects = VisibleObjects::load(database, config, viewer, &candidates, now).await?;

    // PhabricatorFeedStory::getPolicy() is the most open policy.
    let can_see_stories = viewer.is_omnipotent || viewer.is_logged_in() || config.allow_public();

    let mut visible = Vec::new();
    for candidate in candidates {
        let Some(transactions) = objects.transactions(&candidate) else {
            continue;
        };
        let feed: Vec<FeedVisibility> = transactions.iter().map(|(_, feed)| *feed).collect();
        match feed::story_visibility(&feed) {
            FeedVisibility::Visible if can_see_stories => visible.push(VisibleStory {
                object: candidate.object,
                row: candidate.row,
                transactions: transactions
                    .into_iter()
                    .map(|(xaction, feed)| (xaction, feed == FeedVisibility::Hidden))
                    .collect(),
            }),
            FeedVisibility::Unknown => match unported {
                Unported::Fail => {
                    return Err(StoryError::Unsupported(
                        "feed visibility of a transaction is not ported",
                    ));
                }
                Unported::Skip => skipped += 1,
            },
            _ => {}
        }
    }
    Ok(Stories { visible, skipped })
}

/// The rows of [`load_visible()`], failing on what is not ported: what
/// `feed.query_id` needs.
pub async fn visible_stories(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    rows: Vec<StoryRow>,
    now: i64,
) -> Result<Vec<StoryRow>, StoryError> {
    let stories = load_visible(database, config, viewer, rows, now, Unported::Fail).await?;
    Ok(stories.visible.into_iter().map(|story| story.row).collect())
}
