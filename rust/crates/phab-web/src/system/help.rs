//! `/help/documentation/<class>/` (`PhabricatorHelpDocumentationController`)
//! and `/help/keyboardshortcut/` (`PhabricatorHelpKeyboardShortcutController`)
//! as a page. PHP pages load the shortcut dialog with a workflow request,
//! which goes back to PHP.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use serde_json::Value;

use super::{Body, Item, List, PageBuilder, Section, SystemPage, Table, catalog, no_query};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorHelpApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorHelpApplication";

/// `GET /help/documentation/<class>/`.
pub(crate) async fn documentation<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(class): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    no_query(&uri)?;
    let state = state.as_ref();
    let application = catalog::application(&class).ok_or(PageError::NotFound)?;
    if !viewer
        .can_view_applications(state, &[class.as_str()])
        .await?
        .first()
        .copied()
        .unwrap_or(false)
    {
        return Err(PageError::NotFound);
    }
    // `getHelpMenuItems()`, without its leading divider.
    let items = application
        .help_articles
        .iter()
        .map(|article| Item {
            name: article.name.clone(),
            uri: Some(article.href.clone()),
            ..Item::default()
        })
        .collect();
    Ok(PageBuilder::new(format!("{} Help", application.name))
        .icon("fa-book")
        .section(Section::untitled(Body::List(List {
            items,
            empty: "No data available.".to_owned(),
        })))
        .build(state, &viewer, &uri))
}

/// The groups of the shortcut dialog, in order.
const GROUPS: [(&str, &str); 6] = [
    ("default", "Page Shortcuts"),
    ("diff-nav", "Diff Navigation"),
    ("diff-vis", "Hiding Content"),
    ("inline", "Editing Inline Comments"),
    ("xactions", "Comments"),
    ("global", "Global Shortcuts"),
];

/// How keys are shown.
fn stroke(key: &str) -> &str {
    match key {
        "left" => "\u{2190}",
        "right" => "\u{2192}",
        "up" => "\u{2191}",
        "down" => "\u{2193}",
        "return" => "\u{23ce}",
        "tab" => "\u{21e5}",
        "delete" => "\u{232b}",
        other => other,
    }
}

/// `GET /help/keyboardshortcut/?keys=<json>`.
pub(crate) async fn keyboard_shortcuts<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let mut keys = None;
    for (name, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "keys" => keys = Some(value.into_owned()),
            // Dialogs of PHP pages.
            _ => return Err(PageError::Fallback("a workflow request")),
        }
    }
    let mut shortcuts = match keys.as_deref().map(serde_json::from_str::<Value>) {
        None => Vec::new(),
        Some(Ok(Value::Array(list))) => list,
        Some(_) => return Err(PageError::BadRequest("The shortcuts are not valid JSON.")),
    };
    shortcuts.push(serde_json::json!({
        "keys": ["Esc"],
        "description": "Close any dialog, including this one.",
        "group": "global",
    }));
    let mut rows: Vec<Vec<Vec<String>>> = vec![Vec::new(); GROUPS.len()];
    for shortcut in &shortcuts {
        let strokes = shortcut
            .get("keys")
            .and_then(Value::as_array)
            .ok_or(PageError::BadRequest("A shortcut has no keys."))?
            .iter()
            .map(|key| {
                format!(
                    "<kbd>{}</kbd>",
                    escape(stroke(&phab_core::php::string_cast(key)))
                )
            })
            .collect::<Vec<_>>()
            .join(" or ");
        let group = shortcut.get("group").and_then(Value::as_str).unwrap_or("");
        let index = GROUPS
            .iter()
            .position(|(key, _)| *key == group)
            .unwrap_or(0);
        let description = shortcut
            .get("description")
            .map(|description| phab_core::php::string_cast(description).into_owned())
            .unwrap_or_default();
        rows[index].push(vec![strokes, escape(&description)]);
    }
    let mut page = PageBuilder::new("Keyboard Shortcuts").icon("fa-keyboard-o");
    for ((_, name), rows) in GROUPS.iter().zip(rows) {
        if rows.is_empty() {
            continue;
        }
        page = page.section(Section::new(
            *name,
            Body::Table(Table {
                headers: vec!["Keys", "Action"],
                rows,
                empty: String::new(),
            }),
        ));
    }
    Ok(page.build(state.as_ref(), &viewer, &uri))
}
