//! The timeline of a revision: its transactions the viewer may see, with
//! their titles and comments (`DifferentialTransactionView`).
//!
//! Titles are the feed titles (`getTitleForFeed()`): PHP's timeline uses
//! `getTitle()`, which says "this revision" instead of naming it.

use std::collections::HashMap;
use std::fmt::Write;

use phab_app_differential::feed::{self, FeedVisibility};
use phab_app_differential::storage::{self, Comment, Revision};
use phab_app_differential::title::{self, Part, Title};
use phab_app_feed::handles::{self, Handle};
use phab_core::Phid;
use phab_policy::Viewer;
use phab_remarkup::Mode;

use crate::error::PageError;
use crate::state::WebState;
use crate::{remarkup, time};

/// One entry of the timeline.
pub(crate) struct Entry {
    /// The title, as HTML.
    pub title_html: String,
    /// When it happened.
    pub date: String,
    /// The comment, as HTML.
    pub comment_html: Option<String>,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

fn handle_html(handle: Option<&Handle>) -> String {
    match handle {
        Some(Handle {
            name,
            uri: Some(uri),
        }) => format!("<a href=\"{}\">{}</a>", escape(uri), escape(name)),
        Some(Handle { name, uri: None }) => escape(name),
        None => "Unknown Object".to_owned(),
    }
}

/// A title as HTML, with links to what it names.
fn title_html(title: &Title, handles: &HashMap<Phid, Handle>) -> String {
    let mut html = String::new();
    for part in &title.0 {
        match part {
            Part::Text(text) => html.push_str(&escape(text)),
            Part::Handle(phid) => html.push_str(&handle_html(handles.get(phid))),
            Part::HandleList(phids) => {
                let list: Vec<String> = phids
                    .iter()
                    .map(|phid| handle_html(handles.get(phid)))
                    .collect();
                html.push_str(&list.join(", "));
            }
            Part::Value(value) => {
                let _ = write!(html, "&quot;{}&quot;", escape(value));
            }
        }
    }
    html
}

/// The timeline of `revision` for `viewer`.
pub(crate) async fn entries<S: WebState>(
    state: &S,
    viewer: &Viewer,
    revision: &Revision,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let internal = |error: storage::VisibilityError| PageError::Internal(Box::new(error));
    let transactions = storage::load_object_transactions(database, &revision.phid).await?;
    let comment_phids: Vec<Phid> = transactions
        .iter()
        .filter_map(|(transaction, _)| transaction.comment_phid.clone())
        .collect();
    let comments = storage::load_comments(database, &comment_phids).await?;

    // The transactions and comments the viewer may see.
    let subjects: Vec<_> = transactions
        .iter()
        .map(|(transaction, _)| transaction.subject())
        .chain(comments.iter().map(Comment::subject))
        .collect();
    let visible =
        storage::visible_subjects(database, state.config(), viewer, &subjects, state.now())
            .await
            .map_err(internal)?;
    let (visible_transactions, visible_comments) = visible.split_at(transactions.len());
    let comments: HashMap<&Phid, &Comment> = comments
        .iter()
        .zip(visible_comments)
        .filter_map(|(comment, visible)| visible.then_some((&comment.phid, comment)))
        .collect();

    let mut shown = Vec::new();
    for ((transaction, date), visible) in transactions.iter().zip(visible_transactions) {
        if !visible {
            continue;
        }
        let comment = transaction
            .comment_phid
            .as_ref()
            .and_then(|phid| comments.get(phid).copied());
        if feed::feed_visibility(transaction, comment) == FeedVisibility::Hidden {
            continue;
        }
        let title = match title::feed_title(transaction) {
            Ok(Some(title)) if title.handles().iter().all(handles::is_supported) => Some(title),
            Ok(None) => continue,
            Ok(Some(_)) | Err(_) => None,
        };
        shown.push((transaction, *date, title, comment));
    }

    let mut phids: Vec<Phid> = shown
        .iter()
        .filter_map(|(_, _, title, _)| title.as_ref())
        .flat_map(Title::handles)
        .collect();
    phids.sort();
    phids.dedup();
    let handles = handles::load_handles(database, state.config(), viewer, &phids, state.now())
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;

    let mut entries = Vec::new();
    for (_, date, title, comment) in shown {
        let comment_html = match comment.filter(|comment| !comment.content.is_empty()) {
            Some(comment) => {
                let rendered =
                    remarkup::render(state, viewer, &comment.content, Mode::Html).await?;
                Some(String::from_utf8_lossy(rendered.as_bytes()).into_owned())
            }
            None => None,
        };
        entries.push(Entry {
            title_html: title.map_or_else(
                || {
                    "<span class=\"muted\">A change not shown in the new interface yet.</span>"
                        .to_owned()
                },
                |title| title_html(&title, &handles),
            ),
            date: time::format_utc(date),
            comment_html,
        });
    }
    Ok(entries)
}
