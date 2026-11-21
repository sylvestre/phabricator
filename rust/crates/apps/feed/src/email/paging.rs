//! The feed queries of the email methods: `PhabricatorFeedQuery` paged by
//! chronological key (`PhabricatorStory::queryStories()`), and
//! `PhabricatorFeedEmailIDQuery` paged by story ID
//! (`PhabricatorStory::queryStoriesByID()`), both as the omnipotent viewer.

use phab_conduit::pager::{self, Direction, PolicyAwareQuery};
use phab_conduit::{ApiRequest, MethodError};
use phab_db::{Database, DbError, rows};
use phab_policy::Viewer;

use super::Result;
use crate::story::{self, StoryRow, Unported, VisibleStory};

const QUERY_CLASS: &str = "PhabricatorFeedQuery";
/// The largest page `PhabricatorPolicyAwareQuery::execute()` loads.
const MAX_RAW_ROWS: usize = 1024;

/// What a raw page selects.
enum Cut<'a> {
    /// Rows with these chronological keys, compared as strings like
    /// `withChronologicalKeys()` does.
    Keys(&'a [u64]),
    /// Rows past this chronological key, which the paging clause of a
    /// `'type' => 'string'` column quotes.
    KeyAfter(Option<u64>, Direction),
    /// Rows with a larger ID than these (`withMinID()` and the cursor).
    IdAfter(Option<i64>, Option<u64>),
}

/// `loadStandardPageRows()` of the feed queries: stories without
/// references are notifications.
async fn load_rows(
    database: &Database,
    cut: &Cut<'_>,
    limit: Option<usize>,
) -> Result<Vec<StoryRow>, DbError> {
    let mut conditions = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut numbers: Vec<i64> = Vec::new();
    let order = match cut {
        Cut::Keys(keys) => {
            conditions.push(format!(
                "ref.chronologicalKey IN ({})",
                phab_db::placeholders(keys.len())
            ));
            texts.extend(keys.iter().map(u64::to_string));
            "story.chronologicalKey ASC"
        }
        Cut::KeyAfter(after, direction) => {
            let (comparison, order) = match direction {
                Direction::Forward => (">", "story.chronologicalKey ASC"),
                Direction::Backward => ("<", "story.chronologicalKey DESC"),
            };
            if let Some(after) = after {
                conditions.push(format!("story.chronologicalKey {comparison} ?"));
                texts.push(after.to_string());
            }
            order
        }
        Cut::IdAfter(min_id, after) => {
            if let Some(min_id) = min_id {
                conditions.push("story.id > ?".to_owned());
                numbers.push(*min_id);
            }
            if let Some(after) = after {
                conditions.push("story.id > ?".to_owned());
                numbers.push(i64::try_from(*after).unwrap_or(i64::MAX));
            }
            "story.id ASC"
        }
    };
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let limit = limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT story.id, story.phid, story.chronologicalKey, story.storyType, \
         story.storyData, story.authorPHID, story.dateCreated FROM {} story JOIN {} ref \
         ON ref.chronologicalKey = story.chronologicalKey{filter} \
         GROUP BY story.chronologicalKey ORDER BY {order}{limit}",
        database.table("feed", "feed_storydata"),
        database.table("feed", "feed_storyreference"),
    );
    let query = phab_db::bind_all(phab_db::sqlx::query(&sql), texts.iter().map(String::as_str));
    let found = phab_db::bind_all(query, numbers)
        .fetch_all(database.pool())
        .await?;
    found
        .iter()
        .map(|row| {
            Ok(StoryRow {
                id: rows::uint(row, "id")?,
                phid: rows::text(row, "phid")?,
                chronological_key: rows::uint(row, "chronologicalKey")?,
                story_type: rows::text(row, "storyType")?,
                story_data: rows::json(row, "storyData")?,
                author_phid: rows::text(row, "authorPHID")?,
                date_created: rows::int(row, "dateCreated")?,
            })
        })
        .collect()
}

/// `willFilterPage()` as the omnipotent viewer: stories whose objects
/// can't be loaded or which are hidden from feed are dropped.
async fn filter(request: &ApiRequest<'_>, rows: Vec<StoryRow>) -> Result<Vec<VisibleStory>> {
    let context = request.context;
    let stories = story::load_visible(
        &context.database,
        &context.config,
        &Viewer::omnipotent(),
        rows,
        request.now,
        Unported::Fail,
    )
    .await?;
    Ok(stories.visible)
}

/// `PhabricatorFeedQuery` ordered by chronological key.
struct ByKey<'a> {
    request: &'a ApiRequest<'a>,
}

impl PolicyAwareQuery for ByKey<'_> {
    type Row = VisibleStory;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &VisibleStory) -> u64 {
        row.row.chronological_key
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<VisibleStory>, MethodError> {
        let cut = match ids {
            Some(keys) => Cut::Keys(keys),
            None => Cut::KeyAfter(after, direction),
        };
        let rows = load_rows(&self.request.context.database, &cut, limit).await?;
        // The rows go through the filter as they are; `VisibleStory` is
        // the row type so that the pager keeps the filtered stories.
        Ok(rows.into_iter().map(VisibleStory::unfiltered).collect())
    }

    async fn filter(&self, rows: Vec<VisibleStory>) -> Result<Vec<VisibleStory>, MethodError> {
        let rows = rows.into_iter().map(|story| story.row).collect();
        Ok(filter(self.request, rows).await?)
    }

    async fn filter_cursor(
        &self,
        rows: Vec<VisibleStory>,
    ) -> Result<Vec<VisibleStory>, MethodError> {
        let visible = self.filter(rows).await?;
        // The key compares as a number: several rows may match it, which
        // makes executeOne() throw.
        if visible.len() > 1 {
            return Err(MethodError::Unsupported(
                "a cursor matching several stories",
            ));
        }
        Ok(visible)
    }
}

/// `PhabricatorStory::queryStories()`: up to `page_size` stories after the
/// one with chronological key `since`, with `executeWithCursorPager()`.
pub(super) async fn query_by_key(
    request: &ApiRequest<'_>,
    page_size: usize,
    since: Option<i64>,
) -> Result<Vec<VisibleStory>, MethodError> {
    let cursor_value = since.map(serde_json::Value::from);
    let cursor = pager::Cursor::parse(cursor_value.as_ref())?;
    let query = ByKey { request };
    let results = pager::execute(
        &query,
        cursor.as_ref(),
        Direction::Forward,
        page_size.saturating_add(1),
    )
    .await?;
    let paging = pager::Paging {
        size: pager::PageSize {
            shown: page_size.into(),
            value: page_size,
        },
        after: cursor,
        before: None,
    };
    Ok(paging.pager().slice(results).items)
}

/// The stories of `PhabricatorStory::queryStoriesByID()`, and the largest
/// raw ID examined.
pub(super) struct IdPage {
    pub(super) stories: Vec<VisibleStory>,
    pub(super) last_raw_id: Option<u64>,
}

/// `PhabricatorFeedEmailIDQuery` with `setLimit()` and partial results on
/// overheating: `PhabricatorPolicyAwareQuery::execute()`.
pub(super) async fn query_by_id(
    request: &ApiRequest<'_>,
    need: usize,
    min_id: Option<i64>,
) -> Result<IdPage> {
    let database = &request.context.database;
    let overheat_limit = need.saturating_mul(10);
    let mut total_seen = 0;
    let mut stories = Vec::new();
    let mut last_raw_id: Option<u64> = None;
    let mut after = None;
    'pages: loop {
        let limit = (need - stories.len()).min(MAX_RAW_ROWS);
        let rows = load_rows(database, &Cut::IdAfter(min_id, after), Some(limit)).await?;
        let seen = rows.len();
        total_seen += seen;
        if let Some(max) = rows.iter().map(|row| row.id).max() {
            last_raw_id = Some(last_raw_id.map_or(max, |last| last.max(max)));
        }
        if let Some(last) = rows.last() {
            after = Some(last.id);
        }
        for story in filter(request, rows).await? {
            stories.push(story);
            if stories.len() >= need {
                break 'pages;
            }
        }
        if seen < limit || total_seen >= overheat_limit {
            break;
        }
    }
    Ok(IdPage {
        stories,
        last_raw_id,
    })
}

/// `feed.for_email.status`: the newest visible story's key, with
/// `executeOne()`.
pub(super) async fn newest_key(request: &ApiRequest<'_>) -> Result<Option<u64>, MethodError> {
    let query = ByKey { request };
    let stories = pager::execute(&query, None, Direction::Backward, 1).await?;
    Ok(stories.first().map(|story| story.row.chronological_key))
}

/// `feed.for_email.status_id`: `getLastRawID()` of the newest page, which
/// is the largest story ID with references.
pub(super) async fn newest_id(database: &Database) -> Result<Option<u64>, MethodError> {
    let sql = format!(
        "SELECT story.id FROM {} story JOIN {} ref ON ref.chronologicalKey = \
         story.chronologicalKey GROUP BY story.chronologicalKey ORDER BY story.id DESC LIMIT 1",
        database.table("feed", "feed_storydata"),
        database.table("feed", "feed_storyreference"),
    );
    let row = phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await?;
    Ok(row.map(|row| rows::uint(&row, "id")).transpose()?)
}
