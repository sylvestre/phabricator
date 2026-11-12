//! `FeedQueryIDConduitAPIMethod` (moz-extensions), which pages through
//! `PhabricatorFeedIDQuery` with `executeWithCursorPager()`.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::config::is_truthy;
use serde_json::{Value, json};

use crate::{
    APPLICATION,
    story::{Order, Page, StoryRow, load_page, visible_stories},
};

const QUERY_CLASS: &str = "PhabricatorFeedIDQuery";
const DEFAULT_LIMIT: usize = 100;
/// The largest page `PhabricatorPolicyAwareQuery::execute()` loads.
const MAX_RAW_ROWS: usize = 1024;
const ERR_UNKNOWN_TYPE: &str = "ERR-UNKNOWN-TYPE";

pub struct FeedQueryId;

/// The page size, as `AphrontCursorPagerView::setPageSize()` stores it.
#[derive(Debug, Clone, PartialEq)]
struct PageSize {
    /// What `cursor.limit` shows: `max(1, $limit)` keeps a string a string.
    shown: Value,
    value: usize,
}

impl Default for PageSize {
    fn default() -> Self {
        Self {
            shown: DEFAULT_LIMIT.into(),
            value: DEFAULT_LIMIT,
        }
    }
}

impl PageSize {
    /// The `limit` parameter; falsy means the default.
    fn parse(limit: Option<&Value>) -> Result<Self, MethodError> {
        let Some(limit) = limit.filter(|limit| is_truthy(limit)) else {
            return Ok(Self::default());
        };
        let value: i64 = match limit {
            Value::Number(number) => number.as_i64(),
            Value::String(text) if is_integer(text) => text.parse().ok(),
            _ => None,
        }
        .ok_or(MethodError::Unsupported(
            "feed.query_id with an unusual limit",
        ))?;
        // max() returns its first argument when both are equal.
        if value <= 1 {
            return Ok(Self {
                shown: 1.into(),
                value: 1,
            });
        }
        Ok(Self {
            shown: limit.clone(),
            value: usize::try_from(value)
                .map_err(|_| MethodError::Unsupported("feed.query_id with a huge limit"))?,
        })
    }
}

/// An optional `-` followed by digits, which PHP compares as an integer.
fn is_integer(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// An `after` or `before` value given to the pager.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    /// `phutil_string_cast()` of the value.
    text: String,
    /// PHP truthiness of the value, which the pager tests.
    truthy: bool,
}

impl Cursor {
    /// `phutil_nonempty_scalar()` then `phutil_string_cast()`.
    fn parse(value: Option<&Value>) -> Result<Option<Self>, MethodError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let text = match value {
            Value::Null | Value::Bool(false) => return Ok(None),
            Value::String(text) if text.is_empty() => return Ok(None),
            Value::String(text) => text.clone(),
            Value::Bool(true) => "1".to_owned(),
            Value::Number(number) if !number.is_f64() => number.to_string(),
            _ => {
                return Err(MethodError::Unsupported(
                    "feed.query_id with an unusual cursor",
                ));
            }
        };
        Ok(Some(Self {
            text,
            truthy: is_truthy(value),
        }))
    }

    /// `story.id IN (%Ld)` casts the cursor to an integer.
    fn id(&self) -> Result<u64, MethodError> {
        if !self.text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(MethodError::Unsupported(
                "feed.query_id with a non-numeric cursor",
            ));
        }
        self.text
            .parse()
            .map_err(|_| MethodError::Unsupported("feed.query_id with a huge cursor"))
    }
}

/// The `view` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Data,
    /// html, html-summary and text render stories.
    Rendered,
    Unknown,
}

impl View {
    /// Falsy means `data`.
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        match value.filter(|value| is_truthy(value)) {
            None => Ok(Self::Data),
            Some(Value::String(text)) => Ok(match text.as_str() {
                "data" => Self::Data,
                "html" | "html-summary" | "text" => Self::Rendered,
                _ => Self::Unknown,
            }),
            // switch() compares loosely: `true` matches the first case.
            Some(_) => Err(MethodError::Unsupported(
                "feed.query_id with an unusual view",
            )),
        }
    }
}

/// `PhutilNumber`'s default formatting.
fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn core_error(message: String) -> MethodError {
    MethodError::Conduit {
        code: "ERR-CONDUIT-CORE".to_owned(),
        info: Some(message),
    }
}

/// `json_encode(json_decode($data, true))`: PHP has no empty objects, and
/// objects keyed "0", "1"... become lists.
fn php_array(value: &Value) -> Value {
    match value {
        Value::Array(items) => items.iter().map(php_array).collect(),
        Value::Object(entries) => {
            let is_list = entries
                .keys()
                .enumerate()
                .all(|(index, key)| *key == index.to_string());
            if is_list {
                entries.values().map(php_array).collect()
            } else {
                Value::Object(
                    entries
                        .iter()
                        .map(|(key, item)| (key.clone(), php_array(item)))
                        .collect(),
                )
            }
        }
        other => other.clone(),
    }
}

/// The `data` view of a story.
fn story_dictionary(story: &StoryRow) -> Value {
    json!({
        "id": story.id.to_string(),
        "phid": story.phid,
        "class": story.story_type,
        "epoch": story.epoch(),
        "authorPHID": story.author_phid,
        "chronologicalKey": story.chronological_key.to_string(),
        "data": php_array(&story.story_data),
    })
}

/// `PhabricatorPolicyAwareQuery::execute()` with a limit of `need`, then
/// `didLoadResults()`.
async fn execute(
    request: &ApiRequest<'_>,
    external: Option<&Cursor>,
    order: Order,
    need: usize,
) -> Result<Vec<StoryRow>, MethodError> {
    let context = request.context;
    let database = &context.database;
    let filter = async |rows| {
        visible_stories(
            database,
            &context.config,
            &request.viewer,
            rows,
            request.now,
        )
        .await
    };

    // newInternalCursorFromExternalCursor(): the cursor must be a story the
    // viewer can see.
    let mut after = None;
    if let Some(external) = external {
        let id = external.id()?;
        let rows = load_page(
            database,
            Page {
                ids: Some(&[id]),
                ..Page::default()
            },
        )
        .await
        .map_err(MethodError::internal)?;
        if filter(rows).await?.is_empty() {
            return Err(core_error(format!(
                "Cursor \"{}\" does not identify a valid object in query \"{QUERY_CLASS}\".",
                external.text
            )));
        }
        after = Some(id);
    }

    let overheat_limit = need * 10;
    let mut total_seen = 0;
    let mut results = Vec::new();
    'pages: loop {
        let limit = (need - results.len()).min(MAX_RAW_ROWS);
        let rows = load_page(
            database,
            Page {
                ids: None,
                after,
                order,
                limit: Some(limit),
            },
        )
        .await
        .map_err(MethodError::internal)?;
        let seen = rows.len();
        total_seen += seen;
        // nextPage(): paging continues after the last raw row.
        if let Some(last) = rows.last() {
            after = Some(last.id);
        }
        for story in filter(rows).await? {
            results.push(story);
            if results.len() >= need {
                break 'pages;
            }
        }
        if seen < limit {
            break;
        }
        if total_seen >= overheat_limit {
            return Err(core_error(format!(
                "Query (of class \"{QUERY_CLASS}\") overheated: examined more than {} raw rows \
                 without finding {} visible objects.",
                thousands(overheat_limit),
                thousands(need)
            )));
        }
    }
    if order == Order::Newest {
        results.reverse();
    }
    Ok(results)
}

/// A page of results with the cursors to the pages around it.
#[derive(Debug, PartialEq)]
struct Paged<T> {
    items: Vec<T>,
    next: Option<T>,
    previous: Option<T>,
}

/// `AphrontCursorPagerView::sliceResults()` and the cursors
/// `executeWithCursorPager()` sets. `results` holds up to `page + 1` items.
fn slice<T: Clone>(mut results: Vec<T>, page: usize, after: bool, before: bool) -> Paged<T> {
    let more = results.len() > page;
    if more {
        let offset = if before { results.len() - page } else { 0 };
        results.drain(..offset);
        results.truncate(page);
    }
    let next = results.last().filter(|_| before || more).cloned();
    let previous = results
        .first()
        .filter(|_| after || (before && more))
        .cloned();
    Paged {
        items: results,
        next,
        previous,
    }
}

impl Method for FeedQueryId {
    fn name(&self) -> &'static str {
        "feed.query_id"
    }

    fn params(&self) -> &'static [&'static str] {
        &["limit", "after", "before", "view"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[(
            ERR_UNKNOWN_TYPE,
            "Unsupported view type, possibles are: html, data, html-summary, text",
        )]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let view = View::parse(request.value("view"))?;
            let after = Cursor::parse(request.value("after"))?;
            let before = Cursor::parse(request.value("before"))?;
            let size = PageSize::parse(request.value("limit"))?;

            // executeWithCursorPager()
            let (external, order) = match (&after, &before) {
                (Some(after), _) => (Some(after), Order::Oldest),
                (None, Some(before)) => (Some(before), Order::Newest),
                (None, None) => (None, Order::Oldest),
            };
            let stories = execute(request, external, order, size.value + 1).await?;
            let paged = slice(
                stories,
                size.value,
                after.as_ref().is_some_and(|after| after.truthy),
                before.as_ref().is_some_and(|before| before.truthy),
            );

            let data = paged
                .items
                .iter()
                .map(|story| match view {
                    View::Data => Ok(story_dictionary(story)),
                    View::Rendered => Err(MethodError::Unsupported("rendered feed stories")),
                    View::Unknown => Err(MethodError::Conduit {
                        code: ERR_UNKNOWN_TYPE.to_owned(),
                        info: None,
                    }),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let cursor = |story: Option<StoryRow>| story.map(|story| story.id.to_string());
            Ok(json!({
                "data": data,
                "cursor": {
                    "limit": size.shown,
                    "after": cursor(paged.next),
                    "before": cursor(paged.previous),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case(json!(null), json!(100), 100)]
    #[case(json!(0), json!(100), 100)]
    #[case(json!("0"), json!(100), 100)]
    #[case(json!(""), json!(100), 100)]
    #[case(json!(5), json!(5), 5)]
    #[case(json!("5"), json!("5"), 5)]
    #[case(json!("1"), json!(1), 1)]
    #[case(json!(-3), json!(1), 1)]
    #[case(json!("-3"), json!(1), 1)]
    fn limits_like_php(#[case] limit: Value, #[case] shown: Value, #[case] value: usize) {
        assert_eq!(PageSize::parse(Some(&limit)), Ok(PageSize { shown, value }));
    }

    #[rstest]
    #[case(json!("abc"))]
    #[case(json!("1e3"))]
    #[case(json!(2.5))]
    #[case(json!(true))]
    #[case(json!([1]))]
    fn leaves_unusual_limits_to_php(#[case] limit: Value) {
        assert!(matches!(
            PageSize::parse(Some(&limit)),
            Err(MethodError::Unsupported(_))
        ));
    }

    #[test]
    fn reads_cursors_like_php() {
        let cursor = |value: Value| Cursor::parse(Some(&value));
        assert_eq!(Cursor::parse(None), Ok(None));
        assert_eq!(cursor(json!("")), Ok(None));
        assert_eq!(cursor(json!(false)), Ok(None));
        assert_eq!(
            cursor(json!(0)),
            Ok(Some(Cursor {
                text: "0".to_owned(),
                truthy: false
            }))
        );
        assert_eq!(
            cursor(json!("12")),
            Ok(Some(Cursor {
                text: "12".to_owned(),
                truthy: true
            }))
        );
        assert!(cursor(json!([1])).is_err());
        assert!(cursor(json!(1.5)).is_err());
    }

    #[rstest]
    // A first page with more to come.
    #[case(vec![1, 2, 3], 2, false, false, vec![1, 2], Some(2), None)]
    // The last page after a cursor.
    #[case(vec![7], 2, true, false, vec![7], None, Some(7))]
    // Paging backwards keeps the items closest to the cursor.
    #[case(vec![1, 5], 1, false, true, vec![5], Some(5), Some(5))]
    #[case(vec![1], 100, false, true, vec![1], Some(1), None)]
    #[case(vec![], 2, true, true, vec![], None, None)]
    fn slices_like_the_pager(
        #[case] results: Vec<u32>,
        #[case] page: usize,
        #[case] after: bool,
        #[case] before: bool,
        #[case] items: Vec<u32>,
        #[case] next: Option<u32>,
        #[case] previous: Option<u32>,
    ) {
        assert_eq!(
            slice(results, page, after, before),
            Paged {
                items,
                next,
                previous
            }
        );
    }

    #[test]
    fn formats_numbers_like_phutil_number() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(20), "20");
        assert_eq!(thousands(1010), "1,010");
        assert_eq!(thousands(123_456_789), "123,456,789");
    }

    #[test]
    fn reencodes_story_data_like_php() {
        assert_eq!(
            php_array(&json!({"a": {}, "b": {"0": "x", "1": {"k": []}}, "c": {"1": "y"}})),
            json!({"a": [], "b": ["x", {"k": []}], "c": {"1": "y"}})
        );
    }
}
