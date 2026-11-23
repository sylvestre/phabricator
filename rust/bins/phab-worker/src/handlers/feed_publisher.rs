//! `FeedPublisherWorker`, which `PhabricatorFeedStoryPublisher::publish()`
//! queues once it has written a story's `feed_storydata`,
//! `feed_storyreference` and `feed_storynotification` rows: it loads the
//! story (`FeedPushWorker::loadFeedStory()`) and queues a
//! `FeedPublisherHTTPWorker` for each of `feed.http-hooks`. Its only writes
//! are its own archived row and those tasks.
//!
//! Stories Rust can't load as PHP does ([`phab_app_feed::push`]) are
//! declined, before the lease when possible.

use phab_app_feed::push;
use phab_app_feed::story::{StoryError, StoryRow};
use phab_core::Config;
use phab_db::{Database, DbError};
use serde_json::{Value, json};

use crate::handler::{
    Claim, ClaimFuture, Failure, Handler, QueuedTask, Unleased, Work, WorkFuture,
};

/// `FeedPublisherWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FeedPublisherWorker;

/// The class of the follow-up tasks.
pub const HTTP_WORKER: &str = "FeedPublisherHTTPWorker";

/// What a task needs, once read.
struct Loaded {
    /// The key, as the failure message prints it.
    key: String,
    /// The story, if PHP loads it.
    story: Option<StoryRow>,
    /// `feed.http-hooks`.
    hooks: Vec<String>,
}

/// `feed.http-hooks` (`list<string>`, empty by default), as `foreach`
/// walks it; `None` for a value PHP would warn on.
fn http_hooks(config: &Config) -> Option<Vec<String>> {
    let values: Vec<&Value> = match config.value("feed.http-hooks") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(entries)) => entries.values().collect(),
        Some(value) if phab_core::php::is_truthy(value) => return None,
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(|uri| uri.as_str().map(str::to_owned))
        .collect()
}

/// Reads the task data and loads the story; `Err` is how the task ends
/// without it.
async fn load(data: &Value, database: &Database, config: &Config) -> Result<Loaded, Failure> {
    // `$task_data['key']`: anything but a dictionary with an integer or a
    // string there warns or fails in `qsprintf()`.
    let key = match data {
        Value::Object(map) => map.get("key").and_then(push::chronological_key),
        _ => None,
    }
    .ok_or_else(|| Failure::Decline("feed task data without a usable key".to_owned()))?;
    let hooks = http_hooks(config)
        .ok_or_else(|| Failure::Decline("feed.http-hooks is not a list of strings".to_owned()))?;
    let story = push::load_feed_story(database, config, &key, crate::time())
        .await
        .map_err(|error| match error {
            StoryError::Unsupported(reason) => Failure::Decline(reason.to_owned()),
            // Lisk decodes JSON columns without failing.
            StoryError::Database(error @ DbError::Json { .. }) => {
                Failure::Decline(error.to_string())
            }
            other => Failure::Temporary(other.to_string()),
        })?;
    Ok(Loaded { key, story, hooks })
}

impl Handler for FeedPublisherWorker {
    fn class(&self) -> &'static str {
        "FeedPublisherWorker"
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            match load(task.data, task.database, task.config).await {
                Ok(_) => Claim::Take,
                // A query failing now is left to PHP too, rather than counted
                // as a failure of ours.
                Err(failure) => Claim::Decline(failure.to_string()),
            }
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            let loaded = load(work.data(), work.database, work.config).await?;
            let Some(story) = loaded.story else {
                return Err(Failure::Permanent(format!(
                    "Feed story (with key \"{}\") does not exist or could not be loaded.",
                    loaded.key
                )));
            };
            for uri in loaded.hooks {
                // `getChronologicalKey()` of a loaded row is a string.
                work.queue_task(QueuedTask {
                    class: HTTP_WORKER.to_owned(),
                    data: json!({
                        "key": story.chronological_key.to_string(),
                        "uri": uri,
                    }),
                    priority: None,
                    object_phid: None,
                    container_phid: None,
                });
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    fn config(hooks: Value) -> Config {
        let mut config = Config::default();
        let mut layer = Map::new();
        layer.insert("feed.http-hooks".to_owned(), hooks);
        config.push_layer("test", layer);
        config
    }

    #[test]
    fn reads_the_hooks_like_foreach() {
        assert_eq!(http_hooks(&Config::default()), Some(Vec::new()));
        assert_eq!(
            http_hooks(&config(json!(["http://a/", "http://b/"]))),
            Some(vec!["http://a/".to_owned(), "http://b/".to_owned()])
        );
        assert_eq!(
            http_hooks(&config(json!({"x": "http://a/"}))),
            Some(vec!["http://a/".to_owned()])
        );
        assert_eq!(http_hooks(&config(json!(""))), Some(Vec::new()));
        assert_eq!(http_hooks(&config(json!("http://a/"))), None);
        assert_eq!(http_hooks(&config(json!([1]))), None);
    }
}
