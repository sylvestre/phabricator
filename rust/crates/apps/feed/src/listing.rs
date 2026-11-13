//! A page of the feed as `/feed/` shows it (`PhabricatorFeedSearchEngine`
//! with `PhabricatorFeedQuery`): the visible stories newest first, paged by
//! chronological key, each with the title of its primary transaction.

use std::collections::HashMap;

use phab_app_differential::title::{self, Title};
use phab_core::{Config, Phid};
use phab_db::Database;
use phab_policy::Viewer;

use crate::handles::{self, Handle};
use crate::story::{self, Column, Order, Page, StoryError, Unported, VisibleStory};

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
    /// The handles the titles name.
    pub handles: HashMap<Phid, Handle>,
    /// Stories left out because Rust can't render them or decide whether
    /// the viewer may see them.
    pub skipped: usize,
    /// Whether there are older stories than the last one.
    pub has_older: bool,
    /// Whether there are newer stories than the first one.
    pub has_newer: bool,
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

/// Loads a page of the feed for `viewer`.
pub async fn list(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    query: Query<'_>,
    now: i64,
) -> Result<Listing, StoryError> {
    let (order, mut after) = match query.cursor {
        Cursor::Newest => (Order::Newest, None),
        Cursor::After(key) => (Order::Newest, Some(key)),
        Cursor::Before(key) => (Order::Oldest, Some(key)),
    };
    let mut listing = Listing::default();
    let mut exhausted = false;
    for _ in 0..MAX_ROUNDS {
        let rows = story::load_page(
            database,
            Page {
                after,
                order,
                limit: Some(ROUND_ROWS),
                column: Column::ChronologicalKey,
                references: query.references,
                ..Page::default()
            },
        )
        .await?;
        exhausted = rows.len() < ROUND_ROWS;
        after = rows.last().map(|row| row.chronological_key).or(after);
        let stories =
            story::load_visible(database, config, viewer, rows, now, Unported::Skip).await?;
        listing.skipped += stories.skipped;
        for story in stories.visible {
            let Some(primary) = story.primary_transaction() else {
                continue;
            };
            match title::feed_title(primary) {
                Ok(Some(title)) => listing.entries.push(Entry { story, title }),
                // PHP shows the story without a title; there is nothing to
                // show here.
                Ok(None) => {}
                Err(_) => listing.skipped += 1,
            }
        }
        if listing.entries.len() > query.limit || exhausted {
            break;
        }
    }

    let more = listing.entries.len() > query.limit || !exhausted;
    listing.entries.truncate(query.limit);
    match query.cursor {
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

    // Titles naming objects whose handles are not ported (repositories,
    // packages) are left out.
    let before = listing.entries.len();
    listing
        .entries
        .retain(|entry| entry.title.handles().iter().all(handles::is_supported));
    listing.skipped += before - listing.entries.len();

    let mut phids: Vec<Phid> = listing
        .entries
        .iter()
        .flat_map(|entry| entry.title.handles())
        .collect();
    phids.sort();
    phids.dedup();
    listing.handles = handles::load_handles(database, config, viewer, &phids, now).await?;
    Ok(listing)
}
