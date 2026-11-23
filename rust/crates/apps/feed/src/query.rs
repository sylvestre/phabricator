//! `FeedQueryConduitAPIMethod` (`feed.query`), which pages through
//! `PhabricatorFeedQuery` by chronological key, newest first, and answers
//! a dictionary of stories keyed by PHID.
//!
//! Ported: `filterPHIDs`, `limit`, `after`, `before` and the `data` view.
//! Left to PHP: the rendered views (`html`, `html-summary`, `text`) when
//! there are stories, and what [`crate::story::visible_stories()`] leaves
//! to PHP.

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use serde_json::{Map, Value, json};

use crate::query_id::{ERR_UNKNOWN_TYPE, View, php_array};
use crate::story::{Column, Order, Page, StoryRow};
use crate::{APPLICATION, story};

const QUERY_CLASS: &str = "PhabricatorFeedQuery";
const DEFAULT_LIMIT: usize = 100;

/// `feed.query`.
pub struct FeedQuery;

/// `filterPHIDs`: `withFilterPHIDs()` when the value is truthy.
fn filter_phids(value: Option<&Value>) -> Result<Option<Vec<Phid>>, MethodError> {
    let items: Vec<&Value> = match value {
        Some(value) if !php::is_truthy(value) => return Ok(None),
        None => return Ok(None),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(entries)) => entries.values().collect(),
        Some(_) => {
            return Err(MethodError::Unsupported(
                "feed.query with unusual filterPHIDs",
            ));
        }
    };
    items
        .into_iter()
        .map(|item| item.as_str().map(Phid::new))
        .collect::<Option<Vec<Phid>>>()
        .map(Some)
        .ok_or(MethodError::Unsupported(
            "feed.query with unusual filterPHIDs",
        ))
}

/// The `data` view of a story.
fn story_dictionary(story: &StoryRow) -> Value {
    json!({
        "class": story.story_type,
        "epoch": story.epoch(),
        "authorPHID": story.author_phid,
        "chronologicalKey": story.chronological_key.to_string(),
        "data": php_array(&story.story_data),
    })
}

/// `PhabricatorFeedQuery` for the viewer of a request.
struct Stories<'a> {
    request: &'a ApiRequest<'a>,
    references: Option<&'a [Phid]>,
}

impl PolicyAwareQuery for Stories<'_> {
    type Row = StoryRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &StoryRow) -> u64 {
        row.chronological_key
    }

    async fn load(
        &self,
        keys: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<StoryRow>, MethodError> {
        // The query of a cursor (`withChronologicalKeys()`) has no filter.
        let references = if keys.is_some() {
            None
        } else {
            self.references
        };
        let order = match direction {
            Direction::Forward => Order::Newest,
            Direction::Backward => Order::Oldest,
        };
        let page = Page {
            keys,
            after,
            order,
            limit,
            column: Column::ChronologicalKey,
            references,
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

impl Method for FeedQuery {
    fn name(&self) -> &'static str {
        "feed.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &["filterPHIDs", "limit", "after", "before", "view"]
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
            let references = filter_phids(request.value("filterPHIDs"))?;
            let paging = Paging::parse(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
                DEFAULT_LIMIT,
            )?;
            let (start, direction) = paging.start();
            let stories = pager::execute(
                &Stories {
                    request,
                    references: references.as_deref(),
                },
                start,
                direction,
                paging.size.value + 1,
            )
            .await?;
            let page = paging.pager().slice(stories);

            let mut results = Map::new();
            for story in &page.items {
                let data = match view {
                    View::Data => story_dictionary(story),
                    View::Rendered => {
                        return Err(MethodError::Unsupported("rendered feed stories"));
                    }
                    View::Unknown => {
                        return Err(MethodError::Conduit {
                            code: ERR_UNKNOWN_TYPE.to_owned(),
                            info: None,
                        });
                    }
                };
                results.insert(story.phid.clone(), data);
            }
            // An empty PHP array is a list.
            Ok(if results.is_empty() {
                json!([])
            } else {
                Value::Object(results)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn filter_phids_follow_php_truthiness() {
        assert_eq!(filter_phids(None), Ok(None));
        assert_eq!(filter_phids(Some(&json!([]))), Ok(None));
        assert_eq!(filter_phids(Some(&json!(""))), Ok(None));
        assert_eq!(
            filter_phids(Some(&json!(["PHID-USER-a"]))),
            Ok(Some(vec![Phid::new("PHID-USER-a")]))
        );
        assert_eq!(
            filter_phids(Some(&json!({"x": "PHID-USER-b"}))),
            Ok(Some(vec![Phid::new("PHID-USER-b")]))
        );
        assert!(filter_phids(Some(&json!("PHID-USER-a"))).is_err());
        assert!(filter_phids(Some(&json!([1]))).is_err());
    }
}
