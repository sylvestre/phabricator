//! `/feed/`: recent activity (`PhabricatorFeedListController`), and the
//! story lists other pages embed (the profile's recent activity).

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{RawQuery, State};
use axum::http::Uri;
use phab_app_differential::title::{Part, Title};
use phab_app_feed::handles::Handle;
use phab_app_feed::listing::{self, Cursor, Listing, Query};
use phab_app_feed::story::StoryError;
use phab_core::Phid;
use phab_policy::Viewer;

use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// `PhabricatorFeedSearchEngine` shows this many stories a page.
const PAGE_SIZE: usize = 100;

/// A story as the templates show it.
pub(crate) struct StoryItem {
    /// The title, as HTML.
    pub(crate) title_html: String,
    /// When it happened.
    pub(crate) date: String,
}

/// `htmlspecialchars()`.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#039;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn handle_html(phid: &Phid, handles: &HashMap<Phid, Handle>) -> String {
    let Some(handle) = handles.get(phid) else {
        return String::new();
    };
    match &handle.uri {
        Some(uri) => format!("<a href=\"{}\">{}</a>", escape(uri), escape(&handle.name)),
        None => format!(
            "<span class=\"handle-restricted\">{}</span>",
            escape(&handle.name)
        ),
    }
}

/// A title as HTML: handles link to their objects, restricted ones don't.
pub(crate) fn title_html(title: &Title, handles: &HashMap<Phid, Handle>) -> String {
    let mut html = String::new();
    for part in &title.0 {
        match part {
            Part::Text(text) => html.push_str(&escape(text)),
            Part::Handle(phid) => html.push_str(&handle_html(phid, handles)),
            Part::HandleList(list) => {
                let links: Vec<String> =
                    list.iter().map(|phid| handle_html(phid, handles)).collect();
                html.push_str(&links.join(", "));
            }
            Part::Value(value) => {
                html.push_str("<span class=\"title-value\">");
                html.push_str(&escape(value));
                html.push_str("</span>");
            }
        }
    }
    html
}

fn page_error(error: StoryError) -> PageError {
    match error {
        StoryError::Database(error) => error.into(),
        other => PageError::Internal(Box::new(other)),
    }
}

/// The stories of a listing, ready for a template.
pub(crate) fn items(listing: &Listing) -> Vec<StoryItem> {
    listing
        .entries
        .iter()
        .map(|entry| StoryItem {
            title_html: title_html(&entry.title, &listing.handles),
            date: time::format_utc(i64::try_from(entry.story.row.epoch()).unwrap_or(i64::MAX)),
        })
        .collect()
}

/// Loads a page of the feed for `viewer`.
pub(crate) async fn load<S: WebState>(
    state: &S,
    viewer: &Viewer,
    query: Query<'_>,
) -> Result<Listing, PageError> {
    listing::list(state.database(), state.config(), viewer, query, state.now())
        .await
        .map_err(page_error)
}

#[derive(Template)]
#[template(path = "feed.html")]
pub(crate) struct Feed {
    shell: Shell,
    stories: Vec<StoryItem>,
    skipped: usize,
    older: Option<String>,
    newer: Option<String>,
}

/// The parameters of `/feed/`: `after`, `before` and `userPHIDs`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Parameters {
    cursor: Cursor,
    users: Vec<Phid>,
}

fn parameters(query: Option<&str>) -> Parameters {
    let mut parameters = Parameters::default();
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match key.as_ref() {
            "after" => {
                if let Ok(key) = value.parse() {
                    parameters.cursor = Cursor::After(key);
                }
            }
            "before" => {
                if let Ok(key) = value.parse() {
                    parameters.cursor = Cursor::Before(key);
                }
            }
            "userPHIDs" | "userPHIDs[]" => parameters.users.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|phid| !phid.is_empty())
                    .map(Phid::new),
            ),
            _ => {}
        }
    }
    parameters
}

/// The link to another page of the same listing.
fn page_link(cursor: &str, key: u64, users: &[Phid]) -> String {
    let mut link = format!("/feed/?{cursor}={key}");
    if !users.is_empty() {
        let users: Vec<&str> = users.iter().map(Phid::as_str).collect();
        link.push_str("&userPHIDs=");
        link.extend(form_urlencoded::byte_serialize(users.join(",").as_bytes()));
    }
    link
}

/// `GET /feed/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<Feed>, PageError> {
    let parameters = parameters(query.as_deref());
    let listing = load(
        state.as_ref(),
        &viewer.user.viewer(),
        Query {
            cursor: parameters.cursor,
            limit: PAGE_SIZE,
            references: (!parameters.users.is_empty()).then_some(parameters.users.as_slice()),
        },
    )
    .await?;
    let key = |entry: Option<&listing::Entry>| entry.map(|entry| entry.story.row.chronological_key);
    let older = key(listing.entries.last())
        .filter(|_| listing.has_older)
        .map(|key| page_link("after", key, &parameters.users));
    let newer = key(listing.entries.first())
        .filter(|_| listing.has_newer)
        .map(|key| page_link("before", key, &parameters.users));

    let current = uri.path_and_query().map_or("/feed/", |path| path.as_str());
    Ok(Page(Feed {
        shell: Shell::for_user(state.config(), &viewer, "Feed", current),
        stories: items(&listing),
        skipped: listing.skipped,
        older,
        newer,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_link_visible_handles_only() {
        let handles = HashMap::from([
            (
                Phid::new("PHID-USER-alice"),
                Handle {
                    name: "alice".to_owned(),
                    uri: Some("/p/alice/".to_owned()),
                },
            ),
            (
                Phid::new("PHID-DREV-1"),
                Handle {
                    name: "Restricted Differential Revision".to_owned(),
                    uri: None,
                },
            ),
        ]);
        let title = Title(vec![
            Part::Handle(Phid::new("PHID-USER-alice")),
            Part::Text(" retitled ".to_owned()),
            Part::Handle(Phid::new("PHID-DREV-1")),
            Part::Text(" to ".to_owned()),
            Part::Value("<b>".to_owned()),
            Part::Text(".".to_owned()),
        ]);
        assert_eq!(
            title_html(&title, &handles),
            "<a href=\"/p/alice/\">alice</a> retitled \
             <span class=\"handle-restricted\">Restricted Differential Revision</span> to \
             <span class=\"title-value\">&lt;b&gt;</span>."
        );
    }

    #[test]
    fn parameters_follow_php() {
        assert_eq!(
            parameters(Some("after=12&userPHIDs=PHID-USER-a,PHID-USER-b")),
            Parameters {
                cursor: Cursor::After(12),
                users: vec![Phid::new("PHID-USER-a"), Phid::new("PHID-USER-b")],
            }
        );
        assert_eq!(parameters(Some("before=x")), Parameters::default());
        assert_eq!(
            page_link("before", 3, &[Phid::new("PHID-USER-a")]),
            "/feed/?before=3&userPHIDs=PHID-USER-a"
        );
    }
}
