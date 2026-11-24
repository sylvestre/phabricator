//! `/notification/` and its builtin queries (`PhabricatorNotificationListController`
//! with `PhabricatorNotificationSearchEngine`): the viewer's notifications,
//! newest first, the unread ones marked. "Mark All Read" and the menu's
//! panel stay with PHP.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_feed::listing::{self, Cursor, NotificationQuery};

use crate::error::PageError;
use crate::feed::{self, StoryItem};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorNotificationsApplication`.
const APPLICATION: &str = "PhabricatorNotificationsApplication";
/// The page size of `PhabricatorApplicationSearchEngine`.
const PAGE_SIZE: usize = 100;

/// The builtin queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    /// `all`, the default.
    All,
    /// `unread`: only the notifications not viewed yet.
    Unread,
}

impl Builtin {
    const ALL: [Self; 2] = [Self::All, Self::Unread];

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "all" => Some(Self::All),
            "unread" => Some(Self::Unread),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::All => "All Notifications",
            Self::Unread => "Unread Notifications",
        }
    }

    fn uri(self) -> &'static str {
        match self {
            Self::All => "/notification/query/all/",
            Self::Unread => "/notification/query/unread/",
        }
    }

    /// `renderResultList()`'s header.
    fn heading(self) -> &'static str {
        match self {
            Self::All => "Notifications",
            Self::Unread => "Unread Notifications",
        }
    }

    fn no_data(self) -> &'static str {
        match self {
            Self::All => "You have no notifications.",
            Self::Unread => "You have no unread notifications.",
        }
    }
}

/// A notification: its story, and whether it is unread.
pub(crate) struct NotificationItem {
    story: StoryItem,
    unread: bool,
}

#[derive(Template)]
#[template(path = "notifications.html")]
pub(crate) struct NotificationsPage {
    shell: Shell,
    heading: &'static str,
    no_data: &'static str,
    /// The builtin queries: name, link, and whether the page shows it.
    queries: Vec<(&'static str, &'static str, bool)>,
    notifications: Vec<NotificationItem>,
    skipped: usize,
    older: Option<String>,
    newer: Option<String>,
}

/// The `after` and `before` parameters. The search parameters of a query
/// string (`?unread=1`) make PHP build a query of its own: that is left
/// to it.
fn cursor(query: Option<&str>) -> Result<Cursor, PageError> {
    let mut cursor = Cursor::Newest;
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match key.as_ref() {
            "after" => {
                if let Ok(key) = value.parse() {
                    cursor = Cursor::After(key);
                }
            }
            "before" => {
                if let Ok(key) = value.parse() {
                    cursor = Cursor::Before(key);
                }
            }
            "unread" => return Err(PageError::Fallback("a notification query from the URI")),
            _ => {}
        }
    }
    Ok(cursor)
}

async fn render<S: WebState>(
    state: &S,
    viewer: &WebUser,
    builtin: Builtin,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<NotificationsPage>, PageError> {
    viewer.require_application(state, APPLICATION).await?;
    let cursor = cursor(query)?;
    let mut listing = listing::notifications(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        NotificationQuery {
            cursor,
            limit: PAGE_SIZE,
            user: &viewer.user.phid,
            unread: builtin == Builtin::Unread,
        },
        state.now(),
    )
    .await
    .map_err(feed::page_error)?;
    let handles = feed::resolve(state, viewer, &mut listing).await?;
    let key = |entry: Option<&listing::Entry>| entry.map(|entry| entry.story.row.chronological_key);
    let older = key(listing.entries.last())
        .filter(|_| listing.has_older)
        .map(|key| feed::page_link(builtin.uri(), "after", key, &[]));
    let newer = key(listing.entries.first())
        .filter(|_| listing.has_newer)
        .map(|key| feed::page_link(builtin.uri(), "before", key, &[]));
    let notifications = feed::items(&listing, &handles)
        .into_iter()
        .zip(&listing.entries)
        .map(|(story, entry)| NotificationItem {
            story,
            unread: listing.unread.contains(&entry.story.row.chronological_key),
        })
        .collect();

    let current = uri
        .path_and_query()
        .map_or("/notification/", |path| path.as_str());
    Ok(Page(NotificationsPage {
        shell: Shell::for_user(state.config(), viewer, "Notifications", current),
        heading: builtin.heading(),
        no_data: builtin.no_data(),
        queries: Builtin::ALL
            .iter()
            .map(|query| (query.name(), query.uri(), *query == builtin))
            .collect(),
        notifications,
        skipped: listing.skipped,
        older,
        newer,
    }))
}

/// `GET /notification/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<NotificationsPage>, PageError> {
    render(
        state.as_ref(),
        &viewer,
        Builtin::All,
        query.as_deref(),
        &uri,
    )
    .await
}

/// `GET /notification/query/{key}/`: the builtin queries; saved ones go to
/// PHP.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<NotificationsPage>, PageError> {
    let builtin =
        Builtin::from_key(&key).ok_or(PageError::Fallback("a saved notification query"))?;
    render(state.as_ref(), &viewer, builtin, query.as_deref(), &uri).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_are_read_and_queries_left_to_php() {
        assert_eq!(cursor(None).ok(), Some(Cursor::Newest));
        assert_eq!(cursor(Some("after=12")).ok(), Some(Cursor::After(12)));
        assert_eq!(cursor(Some("before=x")).ok(), Some(Cursor::Newest));
        assert!(matches!(
            cursor(Some("unread=1")),
            Err(PageError::Fallback(_))
        ));
    }
}
