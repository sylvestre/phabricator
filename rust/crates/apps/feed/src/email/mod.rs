//! Mozilla's email feed (moz-extensions `src/email/`): the
//! `feed.for_email.*` methods phabricator-emails polls for the events it
//! sends mail about.
//!
//! The events are built like `EmailEventBuilder`; what PHP would throw while
//! building one turns it into a "minimal" event, as PHP's `catch` does
//! ([`Fail::Throw`]).

mod bugzilla;
mod code;
mod comments;
mod events;
mod other;
mod paging;
mod recipients;
mod store;

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::DbError;
use serde_json::{Value, json};

use crate::APPLICATION;
use crate::story::StoryError;
use store::Store;

/// The project whose members may call the methods.
const EMAIL_PROJECT: &str = "mozilla-phabricator-emails";
/// `bugzilla.url` of the local development environment, where anybody may.
const DEVELOPMENT_BUGZILLA: &str = "http://bmo.test";
const DEFAULT_LIMIT: i64 = 100;
/// `FeedForEmailQueryIDAPIMethod::$MAX_LIMIT`.
const MAX_ID_LIMIT: i64 = 1000;

/// Why an answer could not be built.
#[derive(Debug)]
pub(crate) enum Fail {
    /// PHP throws (a `TypeError`, a call on `null`, an exception of
    /// `TransactionList`...). Within the `try` of
    /// `EmailEventBuilder::buildEvents()`, the story gets a minimal event.
    Throw(&'static str),
    /// Only PHP can answer.
    Unsupported(&'static str),
    /// An unexpected failure.
    Internal(String),
}

impl From<DbError> for Fail {
    fn from(error: DbError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<phab_db::sqlx::Error> for Fail {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<StoryError> for Fail {
    fn from(error: StoryError) -> Self {
        match error {
            StoryError::Unsupported(reason) => Self::Unsupported(reason),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<MethodError> for Fail {
    fn from(error: MethodError) -> Self {
        match error {
            MethodError::Unsupported(reason) => Self::Unsupported(reason),
            other => Self::Internal(format!("{other:?}")),
        }
    }
}

impl From<Fail> for MethodError {
    fn from(fail: Fail) -> Self {
        match fail {
            // Outside of the event's `try`, PHP fails the whole call.
            Fail::Throw(reason) | Fail::Unsupported(reason) => Self::Unsupported(reason),
            Fail::Internal(message) => Self::Internal(message),
        }
    }
}

pub(crate) type Result<T, E = Fail> = std::result::Result<T, E>;

/// `EmailAPIAuthorization::assert()`: members of the
/// `mozilla-phabricator-emails` project, or anybody in development.
async fn assert_authorized(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    let database = &request.context.database;
    let projects = phab_app_project::storage::load_by_name(database, EMAIL_PROJECT).await?;
    if projects.len() > 1 {
        return Err(MethodError::Unsupported("several email projects"));
    }
    if let Some(project) = projects.first() {
        if project.parent_phid.is_some() {
            return Err(MethodError::Unsupported("an email project with a parent"));
        }
        let members = store::project_members(database, &project.phid).await?;
        let user = request.user.as_ref().map(|user| &user.phid);
        if user.is_some_and(|user| members.contains(user)) {
            return Ok(());
        }
    }
    if request.context.config.value("bugzilla.url") == Some(&json!(DEVELOPMENT_BUGZILLA)) {
        return Ok(());
    }
    Err(MethodError::Conduit {
        code: "ERR-INVALID-AUTH".to_owned(),
        info: Some(r#"Must be part of the "mozilla-phabricator-emails" security group"#.to_owned()),
    })
}

/// A parameter passed to an `int` (or `?int`) argument: PHP coerces
/// integer strings. `None` for `null`.
fn coerce_int(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_i64().map(Some).ok_or(MethodError::Unsupported(
            "a non-integer email feed parameter",
        )),
        Some(Value::String(text)) if is_integer(text) => text
            .parse()
            .map(Some)
            .map_err(|_| MethodError::Unsupported("a huge email feed parameter")),
        Some(_) => Err(MethodError::Unsupported("an unusual email feed parameter")),
    }
}

fn is_integer(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// The events of `stories` and the response around them, as
/// `json_encode()` returns it: the methods answer with that string.
fn response(events: &[Value], cursor: &Value) -> Value {
    Value::String(phab_core::php::json_encode(&json!({
        "data": {"events": events, "storyErrors": 0},
        "cursor": cursor,
    })))
}

/// `FeedForEmailQueryAPIMethod`: pages by chronological key.
pub struct FeedForEmailQuery;

impl Method for FeedForEmailQuery {
    fn name(&self) -> &'static str {
        "feed.for_email.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &["storyLimit", "after"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            assert_authorized(request).await?;
            let limit = coerce_int(request.value("storyLimit"))?.unwrap_or(DEFAULT_LIMIT);
            let since = coerce_int(request.value("after"))?;
            let page_size = usize::try_from(limit.max(1))
                .map_err(|_| MethodError::Unsupported("a huge storyLimit"))?;
            let stories =
                paging::query_by_key(request, page_size, since.filter(|key| *key != 0)).await?;
            // StoryQueryResult takes a string: a null cursor fails.
            let last_key = match (stories.last(), since) {
                (Some(story), _) => story.row.chronological_key.to_string(),
                (None, Some(since)) => since.to_string(),
                (None, None) => {
                    return Err(MethodError::Unsupported(
                        "an empty email feed without cursor",
                    ));
                }
            };
            let mut store = Store::new(request);
            let events = events::build(&mut store, stories, false).await?;
            Ok(response(
                &events,
                &json!({"limit": limit, "after": last_key}),
            ))
        })
    }
}

/// `FeedForEmailQueryIDAPIMethod`: pages by story ID, past stories which
/// can't be loaded.
pub struct FeedForEmailQueryId;

impl Method for FeedForEmailQueryId {
    fn name(&self) -> &'static str {
        "feed.for_email.query_id"
    }

    fn params(&self) -> &'static [&'static str] {
        &["storyLimit", "after"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            assert_authorized(request).await?;
            // `(int)` then clamped to 1..1000.
            let limit = coerce_int(request.value("storyLimit"))?
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_ID_LIMIT);
            let after = coerce_int(request.value("after"))?;
            let min_id = match after {
                Some(0) | None => None,
                Some(id) => Some(id),
            };
            let page =
                paging::query_by_id(request, usize::try_from(limit).unwrap_or(1), min_id).await?;
            let last_id = match page.last_raw_id {
                Some(id) => i64::try_from(id)
                    .map_err(|_| MethodError::Unsupported("a huge feed story ID"))?,
                None => after.unwrap_or(0),
            };
            let mut store = Store::new(request);
            let events = events::build(&mut store, page.stories, true).await?;
            Ok(response(
                &events,
                &json!({"limit": limit, "after": last_id}),
            ))
        })
    }
}

/// `FeedForEmailStatusAPIMethod`: the chronological key of the newest
/// story.
pub struct FeedForEmailStatus;

impl Method for FeedForEmailStatus {
    fn name(&self) -> &'static str {
        "feed.for_email.status"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            assert_authorized(request).await?;
            match paging::newest_key(request).await? {
                Some(key) => Ok(Value::String(key.to_string())),
                // PHP calls getStoryData() on null.
                None => Err(MethodError::Unsupported("an empty feed")),
            }
        })
    }
}

/// `FeedForEmailStatusIDAPIMethod`: the ID of the newest story, loadable or
/// not.
pub struct FeedForEmailStatusId;

impl Method for FeedForEmailStatusId {
    fn name(&self) -> &'static str {
        "feed.for_email.status_id"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            assert_authorized(request).await?;
            let id = paging::newest_id(&request.context.database).await?;
            Ok(json!(id.unwrap_or(0)))
        })
    }
}

/// `PhabricatorEnv::getProductionURI()` for a path.
pub(crate) fn production_uri(config: &phab_core::Config, path: &str) -> Result<String> {
    let base = match config.value("phabricator.production-uri") {
        Some(value) if phab_core::php::is_truthy(value) => value
            .as_str()
            .ok_or(Fail::Unsupported("an unusual production URI"))?,
        _ => config
            .base_uri()
            .map_err(|error| Fail::Internal(error.to_string()))?,
    };
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// Whether `phid` is of the given PHP prefix, like `PHID-PROJ`.
pub(crate) fn has_prefix(phid: &Phid, prefix: &str) -> bool {
    phid.as_str().starts_with(prefix)
}
