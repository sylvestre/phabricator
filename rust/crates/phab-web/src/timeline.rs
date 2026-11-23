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

/// A row of the transactions of a Herald transcript: the ID (the PHID of
/// a transaction the viewer can't see), the title and the date.
pub(crate) struct TranscriptRow {
    pub(crate) id: String,
    pub(crate) title_html: Option<String>,
    pub(crate) date: Option<String>,
}

/// `HeraldTranscriptController::buildTransactionsTranscriptPanel()` of a
/// revision: the transactions with these PHIDs, in this order.
pub(crate) async fn transcript_rows<S: WebState>(
    state: &S,
    viewer: &Viewer,
    revision: &Phid,
    phids: &[String],
) -> Result<Vec<TranscriptRow>, PageError> {
    let database = state.database();
    let wanted: Vec<Phid> = phids.iter().map(|phid| Phid::new(phid.as_str())).collect();
    let transactions: Vec<(storage::Transaction, i64)> =
        storage::load_object_transactions(database, revision)
            .await?
            .into_iter()
            .filter(|(transaction, _)| wanted.contains(&transaction.phid))
            .collect();
    let subjects: Vec<_> = transactions
        .iter()
        .map(|(transaction, _)| transaction.subject())
        .collect();
    let visible =
        storage::visible_subjects(database, state.config(), viewer, &subjects, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    let ids = storage::load_transaction_ids(database, &wanted).await?;
    let mut found: HashMap<Phid, (i64, Title)> = HashMap::new();
    for ((transaction, date), visible) in transactions.iter().zip(visible) {
        if !visible {
            continue;
        }
        let title = match title::feed_title(transaction) {
            Ok(Some(title)) if title.handles().iter().all(handles::is_supported) => title,
            _ => return Err(PageError::Fallback("a transaction title not ported")),
        };
        found.insert(transaction.phid.clone(), (*date, title));
    }
    let mut handle_phids: Vec<Phid> = found
        .values()
        .flat_map(|(_, title)| title.handles())
        .collect();
    handle_phids.sort();
    handle_phids.dedup();
    let handles =
        handles::load_handles(database, state.config(), viewer, &handle_phids, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    Ok(wanted
        .iter()
        .map(|phid| match (found.get(phid), ids.get(phid)) {
            (Some((date, title)), Some(id)) => TranscriptRow {
                id: id.to_string(),
                title_html: Some(title_html(title, &handles)),
                date: Some(time::format_utc(*date)),
            },
            _ => TranscriptRow {
                id: phid.as_str().to_owned(),
                title_html: None,
                date: None,
            },
        })
        .collect())
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

    let texts: Vec<&str> = shown
        .iter()
        .filter_map(|(_, _, _, comment)| comment.as_ref())
        .map(|comment| comment.content.as_str())
        .filter(|content| !content.is_empty())
        .collect();
    let mut rendered = remarkup::render_all(state, viewer, &texts, Mode::Html)
        .await?
        .into_iter();

    let mut entries = Vec::new();
    for (_, date, title, comment) in shown {
        let comment_html = comment
            .filter(|comment| !comment.content.is_empty())
            .and_then(|_| rendered.next())
            .map(|markup| String::from_utf8_lossy(markup.as_bytes()).into_owned());
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
