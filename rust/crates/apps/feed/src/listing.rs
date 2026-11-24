//! A page of the feed as `/feed/` shows it (`PhabricatorFeedSearchEngine`
//! with `PhabricatorFeedQuery`), or of a user's notifications as
//! `/notification/` shows them (`PhabricatorNotificationSearchEngine`): the
//! visible stories newest first, paged by chronological key, each with the
//! title of its primary transaction.

use std::collections::HashSet;

use phab_app_differential::title::{self, Title};
use phab_core::{Config, Phid, PhidType};
use phab_db::Database;
use phab_policy::Viewer;

use crate::story::{
    self, Column, NotificationPage, Order, Page, StoryError, StoryRow, Unported, VisibleStory,
};

/// How many raw rows a round loads.
const ROUND_ROWS: usize = 100;
/// How many rounds a page may take before it stops looking, like the
/// overheating of policy-aware queries.
const MAX_ROUNDS: usize = 10;

/// Where a page starts (`AphrontCursorPagerView`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Cursor {
    /// The newest stories.
    #[default]
    Newest,
    /// Stories older than this chronological key (`?after=`).
    After(u64),
    /// Stories newer than this chronological key (`?before=`).
    Before(u64),
}

/// A story with its title.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The story.
    pub story: VisibleStory,
    /// The title of its primary transaction.
    pub title: Title,
}

/// A page of the feed.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    /// The stories, newest first.
    pub entries: Vec<Entry>,
    /// Stories left out because Rust can't render them or decide whether
    /// the viewer may see them.
    pub skipped: usize,
    /// Whether there are older stories than the last one.
    pub has_older: bool,
    /// Whether there are newer stories than the first one.
    pub has_newer: bool,
    /// The chronological keys of the notifications the user has not
    /// viewed yet (`hasViewed`); empty for the feed.
    pub unread: HashSet<u64>,
}

/// What a page shows.
#[derive(Debug, Clone, Copy)]
pub struct Query<'a> {
    /// Where the page starts.
    pub cursor: Cursor,
    /// How many stories it shows.
    pub limit: usize,
    /// Only stories referencing one of these (`?userPHIDs=`).
    pub references: Option<&'a [Phid]>,
}

/// What a page of notifications shows.
#[derive(Debug, Clone, Copy)]
pub struct NotificationQuery<'a> {
    /// Where the page starts.
    pub cursor: Cursor,
    /// How many stories it shows.
    pub limit: usize,
    /// Whose notifications.
    pub user: &'a Phid,
    /// Only the notifications not viewed yet.
    pub unread: bool,
}

/// Where the rows of a page come from.
#[derive(Debug, Clone, Copy)]
enum Source<'a> {
    /// `PhabricatorFeedQuery`.
    Feed { references: Option<&'a [Phid]> },
    /// `PhabricatorNotificationQuery`.
    Notifications { user: &'a Phid, unread: bool },
}

impl Source<'_> {
    /// A round of rows, with the keys of those not viewed yet.
    async fn load(
        self,
        database: &Database,
        after: Option<u64>,
        order: Order,
    ) -> Result<(Vec<StoryRow>, HashSet<u64>), StoryError> {
        match self {
            Self::Feed { references } => {
                let rows = story::load_page(
                    database,
                    Page {
                        after,
                        order,
                        limit: Some(ROUND_ROWS),
                        column: Column::ChronologicalKey,
                        references,
                        ..Page::default()
                    },
                )
                .await?;
                Ok((rows, HashSet::new()))
            }
            Self::Notifications { user, unread } => {
                let rows = story::load_notification_page(
                    database,
                    NotificationPage {
                        user,
                        unread,
                        after,
                        order,
                        limit: ROUND_ROWS,
                    },
                )
                .await?;
                let unviewed = rows
                    .iter()
                    .filter(|(_, viewed)| !viewed)
                    .map(|(row, _)| row.chronological_key)
                    .collect();
                Ok((rows.into_iter().map(|(row, _)| row).collect(), unviewed))
            }
        }
    }
}

/// Loads a page of the feed for `viewer`.
pub async fn list(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    query: Query<'_>,
    now: i64,
) -> Result<Listing, StoryError> {
    let source = Source::Feed {
        references: query.references,
    };
    list_from(
        database,
        config,
        viewer,
        source,
        (query.cursor, query.limit),
        now,
    )
    .await
}

/// Loads a page of a user's notifications for `viewer`.
pub async fn notifications(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    query: NotificationQuery<'_>,
    now: i64,
) -> Result<Listing, StoryError> {
    let source = Source::Notifications {
        user: query.user,
        unread: query.unread,
    };
    list_from(
        database,
        config,
        viewer,
        source,
        (query.cursor, query.limit),
        now,
    )
    .await
}

/// The story with this chronological key, as `/feed/<key>/` shows it
/// (`withChronologicalKeys()`): a listing of one entry, or none when the
/// viewer can't see it, and nothing else when Rust can't show it.
pub async fn story(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    key: u64,
    now: i64,
) -> Result<Listing, StoryError> {
    let rows = story::load_page(
        database,
        Page {
            keys: Some(&[key]),
            column: Column::ChronologicalKey,
            ..Page::default()
        },
    )
    .await?;
    let mut listing = Listing::default();
    // PHP shows a story without a title too, which Rust doesn't.
    listing.skipped += add_entries(database, config, viewer, rows, now, &mut listing).await?;
    Ok(listing)
}

/// Filters `rows` for `viewer` and adds the stories it can see with their
/// titles to `listing`. Returns how many visible stories have no title.
async fn add_entries(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    rows: Vec<StoryRow>,
    now: i64,
    listing: &mut Listing,
) -> Result<usize, StoryError> {
    let stories = story::load_visible(database, config, viewer, rows, now, Unported::Skip).await?;
    listing.skipped += stories.skipped;
    let mut untitled = 0;
    for story in stories.visible {
        let Some(primary) = story.primary_transaction() else {
            untitled += 1;
            continue;
        };
        let title = match story.object.phid_type() {
            PhidType::COMMIT => title::audit_feed_title(primary),
            PhidType::PROJECT => title::project_feed_title(primary),
            PhidType::APPLICATION => title::application_feed_title(primary),
            _ => title::feed_title(primary),
        };
        match title {
            Ok(Some(title)) => listing.entries.push(Entry { story, title }),
            // PHP shows the story without a title; there is nothing to
            // show here.
            Ok(None) => untitled += 1,
            Err(_) => listing.skipped += 1,
        }
    }
    Ok(untitled)
}

impl Listing {
    /// Leaves out the entries whose titles name an object `supported`
    /// refuses (whose handle the caller can't load), counting them as
    /// skipped.
    pub fn retain_handles(&mut self, supported: impl Fn(&Phid) -> bool) {
        self.retain_titles(|title| title.handles().iter().all(&supported));
    }

    /// Leaves out the entries whose titles `supported` refuses (which the
    /// caller can't render), counting them as skipped.
    pub fn retain_titles(&mut self, supported: impl Fn(&Title) -> bool) {
        let before = self.entries.len();
        self.entries.retain(|entry| supported(&entry.title));
        self.skipped += before - self.entries.len();
    }

    /// The objects the titles name, sorted.
    pub fn handle_phids(&self) -> Vec<Phid> {
        let mut phids: Vec<Phid> = self
            .entries
            .iter()
            .flat_map(|entry| entry.title.handles())
            .collect();
        phids.sort();
        phids.dedup();
        phids
    }
}

async fn list_from(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    source: Source<'_>,
    (cursor, limit): (Cursor, usize),
    now: i64,
) -> Result<Listing, StoryError> {
    let (order, mut after) = match cursor {
        Cursor::Newest => (Order::Newest, None),
        Cursor::After(key) => (Order::Newest, Some(key)),
        Cursor::Before(key) => (Order::Oldest, Some(key)),
    };
    let mut listing = Listing::default();
    let mut exhausted = false;
    for _ in 0..MAX_ROUNDS {
        let (rows, unread) = source.load(database, after, order).await?;
        exhausted = rows.len() < ROUND_ROWS;
        after = rows.last().map(|row| row.chronological_key).or(after);
        listing.unread.extend(unread);
        add_entries(database, config, viewer, rows, now, &mut listing).await?;
        if listing.entries.len() > limit || exhausted {
            break;
        }
    }

    let more = listing.entries.len() > limit || !exhausted;
    listing.entries.truncate(limit);
    match cursor {
        Cursor::Newest => listing.has_older = more,
        Cursor::After(_) => {
            listing.has_older = more;
            listing.has_newer = true;
        }
        Cursor::Before(_) => {
            listing.entries.reverse();
            listing.has_newer = more;
            listing.has_older = true;
        }
    }
    Ok(listing)
}
