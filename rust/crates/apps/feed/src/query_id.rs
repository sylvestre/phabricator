//! `FeedQueryIDConduitAPIMethod` (moz-extensions), which pages through
//! `PhabricatorFeedIDQuery` with `executeWithCursorPager()`.

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use serde_json::{Value, json};

use crate::story::{Order, Page, StoryRow};
use crate::{APPLICATION, story};
use phab_core::php;

const QUERY_CLASS: &str = "PhabricatorFeedIDQuery";
const DEFAULT_LIMIT: usize = 100;
pub(crate) const ERR_UNKNOWN_TYPE: &str = "ERR-UNKNOWN-TYPE";

/// `feed.query_id`.
pub struct FeedQueryId;

/// The `view` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Data,
    /// html, html-summary and text render stories.
    Rendered,
    Unknown,
}

impl View {
    /// Falsy means `data`.
    pub(crate) fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        match value.filter(|value| php::is_truthy(value)) {
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

/// `json_encode(json_decode($data, true))`: PHP has no empty objects, and
/// objects keyed "0", "1"... become lists.
pub(crate) fn php_array(value: &Value) -> Value {
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

/// `PhabricatorFeedIDQuery` for the viewer of a request.
struct Stories<'a> {
    request: &'a ApiRequest<'a>,
}

impl PolicyAwareQuery for Stories<'_> {
    type Row = StoryRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &StoryRow) -> u64 {
        row.id
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<StoryRow>, MethodError> {
        let order = match direction {
            Direction::Forward => Order::Oldest,
            Direction::Backward => Order::Newest,
        };
        let page = Page {
            ids,
            after,
            order,
            limit,
            ..Page::default()
        };
        Ok(story::load_page(&self.request.context.database, page).await?)
    }

    async fn filter(&self, rows: Vec<StoryRow>) -> Result<Vec<StoryRow>, MethodError> {
        let context = self.request.context;
        Ok(story::visible_stories(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
        )
        .await?)
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
            let paging = Paging::parse(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
                DEFAULT_LIMIT,
            )?;
            let (start, direction) = paging.start();
            let stories = pager::execute(
                &Stories { request },
                start,
                direction,
                paging.size.value + 1,
            )
            .await?;
            let page = paging.pager().slice(stories);

            let data = page
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
                    "limit": paging.size.shown,
                    "after": cursor(page.next),
                    "before": cursor(page.previous),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reencodes_story_data_like_php() {
        assert_eq!(
            php_array(&json!({"a": {}, "b": {"0": "x", "1": {"k": []}}, "c": {"1": "y"}})),
            json!({"a": [], "b": ["x", {"k": []}], "c": {"1": "y"}})
        );
    }
}
