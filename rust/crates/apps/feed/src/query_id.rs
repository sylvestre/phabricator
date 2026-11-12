//! `FeedQueryIDConduitAPIMethod` (moz-extensions), which pages through
//! `PhabricatorFeedIDQuery` with `executeWithCursorPager()`.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::config::is_truthy;
use serde_json::{Map, Value};

use crate::{
    APPLICATION,
    story::{Page, StoryRow, load_page, visible_stories},
};

const QUERY_CLASS: &str = "PhabricatorFeedIDQuery";
const DEFAULT_LIMIT: i64 = 100;
/// The largest page `PhabricatorPolicyAwareQuery::execute()` loads.
const MAX_RAW_ROWS: i64 = 1024;
const ERR_UNKNOWN_TYPE: &str = "ERR-UNKNOWN-TYPE";

pub struct FeedQueryId;

/// The page size, as `AphrontCursorPagerView::setPageSize()` stores it.
#[derive(Debug, Clone, PartialEq)]
struct PageSize {
    /// What `cursor.limit` shows: `max(1, $limit)` keeps a string a string.
    shown: Value,
    value: i64,
}

fn page_size(limit: Option<&Value>) -> Result<PageSize, MethodError> {
    let limit = match limit {
        Some(limit) if is_truthy(limit) => limit,
        _ => {
            return Ok(PageSize {
                shown: DEFAULT_LIMIT.into(),
                value: DEFAULT_LIMIT,
            });
        }
    };
    let value = match limit {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => {
            let digits = text.strip_prefix('-').unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                None
            } else {
                text.parse().ok()
            }
        }
        _ => None,
    }
    .ok_or(MethodError::Unsupported(
        "feed.query_id with an unusual limit",
    ))?;
    // max() returns its first argument when both are equal.
    if value > 1 {
        Ok(PageSize {
            shown: limit.clone(),
            value,
        })
    } else {
        Ok(PageSize {
            shown: 1.into(),
            value: 1,
        })
    }
}

/// An `after` or `before` value given to the pager.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    /// `phutil_string_cast()` of the value.
    text: String,
    /// PHP truthiness of the value, which the pager tests.
    truthy: bool,
}

/// `phutil_nonempty_scalar()` then `phutil_string_cast()`.
fn cursor(value: Option<&Value>) -> Result<Option<Cursor>, MethodError> {
    let text = match value {
        None | Some(Value::Null | Value::Bool(false)) => return Ok(None),
        Some(Value::String(text)) if text.is_empty() => return Ok(None),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "1".to_owned(),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => number.to_string(),
        Some(_) => {
            return Err(MethodError::Unsupported(
                "feed.query_id with an unusual cursor",
            ));
        }
    };
    Ok(Some(Cursor {
        truthy: value.is_some_and(is_truthy),
        text,
    }))
}

/// The `view` parameter, falsy meaning `data`.
enum View {
    Data,
    /// html, html-summary and text render stories.
    Rendered,
    Unknown,
}

fn view(value: Option<&Value>) -> Result<View, MethodError> {
    match value {
        Some(value) if is_truthy(value) => match value {
            Value::String(text) => Ok(match text.as_str() {
                "data" => View::Data,
                "html" | "html-summary" | "text" => View::Rendered,
                _ => View::Unknown,
            }),
            // switch() compares loosely: `true` matches the first case.
            _ => Err(MethodError::Unsupported(
                "feed.query_id with an unusual view",
            )),
        },
        _ => Ok(View::Data),
    }
}

/// `PhutilNumber`'s default formatting.
fn thousands(number: i64) -> String {
    let digits = number.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if number < 0 {
        grouped.insert(0, '-');
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
        Value::Array(items) => Value::Array(items.iter().map(php_array).collect()),
        Value::Object(entries) => {
            let is_list = entries
                .keys()
                .enumerate()
                .all(|(index, key)| *key == index.to_string());
            if is_list {
                Value::Array(entries.values().map(php_array).collect())
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
    let mut dictionary = Map::new();
    dictionary.insert("id".to_owned(), story.id.to_string().into());
    dictionary.insert("phid".to_owned(), story.phid.clone().into());
    dictionary.insert("class".to_owned(), story.story_type.clone().into());
    dictionary.insert("epoch".to_owned(), story.epoch().into());
    dictionary.insert("authorPHID".to_owned(), story.author_phid.clone().into());
    dictionary.insert(
        "chronologicalKey".to_owned(),
        story.chronological_key.to_string().into(),
    );
    dictionary.insert("data".to_owned(), php_array(&story.story_data));
    Value::Object(dictionary)
}

/// The cursor id: `story.id IN (%Ld)` casts the cursor to an integer.
fn cursor_id(cursor: &Cursor) -> Result<u64, MethodError> {
    if cursor.text.bytes().all(|byte| byte.is_ascii_digit()) {
        cursor
            .text
            .parse()
            .map_err(|_| MethodError::Unsupported("feed.query_id with a huge cursor"))
    } else {
        Err(MethodError::Unsupported(
            "feed.query_id with a non-numeric cursor",
        ))
    }
}

/// `PhabricatorPolicyAwareQuery::execute()` with a limit of `need`, then
/// `didLoadResults()`.
async fn execute(
    request: &ApiRequest<'_>,
    external: Option<&Cursor>,
    reversed: bool,
    need: i64,
) -> Result<Vec<StoryRow>, MethodError> {
    let context = request.context;
    let database = &context.database;
    let filter = |rows| {
        visible_stories(
            database,
            &context.config,
            &request.viewer,
            rows,
            request.now,
        )
    };

    // newInternalCursorFromExternalCursor(): the cursor must be a story the
    // viewer can see.
    let mut after = None;
    if let Some(external) = external {
        let id = cursor_id(external)?;
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
        let raw_limit = (need - i64::try_from(results.len()).unwrap_or(i64::MAX)).min(MAX_RAW_ROWS);
        let limit = usize::try_from(raw_limit).unwrap_or(0);
        let rows = load_page(
            database,
            Page {
                ids: None,
                after,
                reversed,
                limit: Some(limit),
            },
        )
        .await
        .map_err(MethodError::internal)?;
        let seen = rows.len();
        total_seen += i64::try_from(seen).unwrap_or(i64::MAX);
        // nextPage(): paging continues after the last raw row.
        if let Some(last) = rows.last() {
            after = Some(last.id);
        }
        for story in filter(rows).await? {
            results.push(story);
            if i64::try_from(results.len()).unwrap_or(i64::MAX) >= need {
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
    if reversed {
        results.reverse();
    }
    Ok(results)
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
            let view = view(request.value("view"))?;
            let after = cursor(request.value("after"))?;
            let before = cursor(request.value("before"))?;
            let size = page_size(request.value("limit"))?;

            // executeWithCursorPager()
            let (external, reversed) = match (&after, &before) {
                (Some(after), _) => (Some(after), false),
                (None, Some(before)) => (Some(before), true),
                (None, None) => (None, false),
            };
            let mut stories = execute(request, external, reversed, size.value + 1).await?;
            let count = stories.len();
            let page = usize::try_from(size.value).unwrap_or(usize::MAX);
            let before_truthy = before.as_ref().is_some_and(|before| before.truthy);
            let after_truthy = after.as_ref().is_some_and(|after| after.truthy);
            if count > page {
                let offset = if before_truthy { count - page } else { 0 };
                stories = stories.split_off(offset);
                stories.truncate(page);
            }
            let mut next = Value::Null;
            let mut previous = Value::Null;
            if let (Some(head), Some(last)) = (stories.first(), stories.last()) {
                if before_truthy || count > page {
                    next = last.id.to_string().into();
                }
                if after_truthy || (before_truthy && count > page) {
                    previous = head.id.to_string().into();
                }
            }

            let mut data = Vec::with_capacity(stories.len());
            for story in &stories {
                match view {
                    View::Data => data.push(story_dictionary(story)),
                    View::Rendered => {
                        return Err(MethodError::Unsupported("rendered feed stories"));
                    }
                    View::Unknown => {
                        return Err(MethodError::Conduit {
                            code: ERR_UNKNOWN_TYPE.to_owned(),
                            info: None,
                        });
                    }
                }
            }

            let mut cursor = Map::new();
            cursor.insert("limit".to_owned(), size.shown);
            cursor.insert("after".to_owned(), next);
            cursor.insert("before".to_owned(), previous);
            let mut result = Map::new();
            result.insert("data".to_owned(), Value::Array(data));
            result.insert("cursor".to_owned(), Value::Object(cursor));
            Ok(Value::Object(result))
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
    fn limits_like_php(#[case] limit: Value, #[case] shown: Value, #[case] value: i64) {
        assert_eq!(page_size(Some(&limit)), Ok(PageSize { shown, value }));
    }

    #[rstest]
    #[case(json!("abc"))]
    #[case(json!("1e3"))]
    #[case(json!(2.5))]
    #[case(json!(true))]
    #[case(json!([1]))]
    fn leaves_unusual_limits_to_php(#[case] limit: Value) {
        assert!(matches!(
            page_size(Some(&limit)),
            Err(MethodError::Unsupported(_))
        ));
    }

    #[test]
    fn reads_cursors_like_php() {
        assert_eq!(cursor(None), Ok(None));
        assert_eq!(cursor(Some(&json!(""))), Ok(None));
        assert_eq!(cursor(Some(&json!(false))), Ok(None));
        assert_eq!(
            cursor(Some(&json!(0))),
            Ok(Some(Cursor {
                text: "0".to_owned(),
                truthy: false
            }))
        );
        assert_eq!(
            cursor(Some(&json!("12"))),
            Ok(Some(Cursor {
                text: "12".to_owned(),
                truthy: true
            }))
        );
        assert!(cursor(Some(&json!([1]))).is_err());
    }

    #[test]
    fn formats_numbers_like_phutil_number() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1010), "1,010");
        assert_eq!(thousands(123_456_789), "123,456,789");
        assert_eq!(thousands(-1000), "-1,000");
    }

    #[test]
    fn reencodes_story_data_like_php() {
        assert_eq!(
            php_array(&json!({"a": {}, "b": {"0": "x", "1": {"k": []}}, "c": {"1": "y"}})),
            json!({"a": [], "b": ["x", {"k": []}], "c": {"1": "y"}})
        );
    }
}
