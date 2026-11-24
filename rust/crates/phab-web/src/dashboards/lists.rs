//! The builtin queries of the dashboard and panel lists (`/dashboard/`,
//! `/dashboard/query/<key>/`, `/dashboard/panel/`,
//! `/dashboard/panel/query/<key>/`: `PhabricatorDashboardListController`
//! and `PhabricatorDashboardPanelListController`), and the panel page
//! (`/W<id>`, `PhabricatorDashboardPanelViewController`). Saved queries,
//! search parameters and panel timelines with transactions whose titles
//! aren't ported (the text, tabs and queries of panels) go to PHP.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_dashboard::storage::{self, Dashboard, ListFilter, Panel, Policied, Transaction};
use phab_app_differential::storage as differential;
use phab_core::{Phid, PhidType};
use phab_policy::Viewer;
use serde_json::Value;

use super::{APPLICATION, HeaderMode, PanelRenderer, Panels, internal};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Action, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::timeline::Entry;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

pub(super) fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// The rows of a list page the viewer sees, newest first, as
/// `PhabricatorCursorPagedPolicyAwareQuery` pages them: up to `PAGE_SIZE`,
/// and the cursor of the next page.
pub(super) async fn visible_page<S, T, F, L>(
    state: &S,
    viewer: &Viewer,
    (after, id): (Option<i64>, fn(&T) -> i64),
    load: L,
) -> Result<(Vec<T>, Option<i64>), PageError>
where
    S: WebState,
    T: Policied,
    F: Future<Output = Result<Vec<T>, phab_db::DbError>>,
    L: Fn(Option<i64>, usize) -> F,
{
    let mut shown: Vec<T> = Vec::new();
    let mut before = after;
    let mut loaded = 0;
    loop {
        let rows = load(before, PAGE_SIZE + 1).await?;
        let exhausted = rows.len() <= PAGE_SIZE;
        loaded += rows.len();
        let last = rows.last().map(id);
        let visible = storage::visible(state.database(), state.config(), viewer, rows, state.now())
            .await
            .map_err(internal)?;
        shown.extend(visible);
        before = last;
        if shown.len() > PAGE_SIZE || exhausted {
            break;
        }
        if loaded >= PAGE_SIZE * 10 {
            // PHP's query overheats.
            return Err(PageError::Fallback("lists the viewer mostly can't see"));
        }
    }
    let next = (shown.len() > PAGE_SIZE).then(|| id(&shown[PAGE_SIZE - 1]));
    shown.truncate(PAGE_SIZE);
    Ok((shown, next))
}

/// A dashboard or panel of a list.
pub(crate) struct ListItem {
    monogram: Option<String>,
    name: String,
    uri: String,
    icon: String,
    archived: bool,
    author_html: Option<String>,
    kind: Option<&'static str>,
}

#[derive(Template)]
#[template(path = "dashboard_list.html")]
pub(crate) struct ListPage {
    shell: Shell,
    /// `dashboard` or `dashboard/panel`.
    base: &'static str,
    heading: &'static str,
    query: &'static str,
    /// The builtin queries: key and name.
    queries: Vec<(&'static str, &'static str)>,
    items: Vec<ListItem>,
    next: Option<i64>,
}

/// Which list a page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dashboards,
    Panels,
}

impl Kind {
    fn engine(self) -> &'static str {
        match self {
            Self::Dashboards => "PhabricatorDashboardSearchEngine",
            Self::Panels => "PhabricatorDashboardPanelSearchEngine",
        }
    }

    /// `getBuiltinQueryNames()`, for a logged-in viewer.
    fn queries(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::Dashboards => vec![
                ("authored", "Authored"),
                ("open", "Active Dashboards"),
                ("all", "All Dashboards"),
            ],
            Self::Panels => vec![
                ("authored", "Authored"),
                ("active", "Active Panels"),
                ("all", "All Panels"),
            ],
        }
    }
}

/// The type names of panels (`getPanelTypeName()`).
fn panel_type_name(panel_type: &str) -> Option<&'static str> {
    Some(match panel_type {
        "text" => "Text Panel",
        "query" => "Query Panel",
        "tabs" => "Tab Panel",
        _ => return None,
    })
}

async fn list_page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (kind, key): (Kind, &str),
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<ListPage>, PageError> {
    let queries = kind.queries();
    let (key, _) = *queries
        .iter()
        .find(|(known, _)| *known == key)
        .ok_or(PageError::Fallback("a saved search query"))?;
    let after = crate::herald::cursor(query)?;
    let filter = ListFilter {
        author: (key == "authored").then(|| viewer.user.phid.clone()),
        active_only: matches!(key, "open" | "active"),
    };
    let as_viewer = viewer.user.viewer();
    let database = state.database();
    let (items, next) = match kind {
        Kind::Dashboards => {
            let id: fn(&Dashboard) -> i64 = |dashboard| dashboard.id;
            let (dashboards, next) =
                visible_page(state, &as_viewer, (after, id), |before, limit| {
                    storage::dashboard_page(database, &filter, (before, limit))
                })
                .await?;
            let authors: Vec<Phid> = dashboards
                .iter()
                .map(|dashboard| dashboard.author_phid.clone())
                .collect();
            let scope = Scope::new(state, viewer);
            let handles = harbormaster::load_handles(state, &scope, &authors).await?;
            let items = dashboards
                .iter()
                .map(|dashboard: &Dashboard| ListItem {
                    monogram: None,
                    name: dashboard.name.clone(),
                    uri: dashboard.uri(),
                    icon: dashboard.icon.clone(),
                    archived: dashboard.is_archived(),
                    author_html: Some(
                        harbormaster::handle(&handles, &dashboard.author_phid).link_html(),
                    ),
                    kind: None,
                })
                .collect();
            (items, next)
        }
        Kind::Panels => {
            let id: fn(&Panel) -> i64 = |panel| panel.id;
            let (panels, next) = visible_page(state, &as_viewer, (after, id), |before, limit| {
                storage::panel_page(database, &filter, (before, limit))
            })
            .await?;
            let items = panels
                .iter()
                .map(|panel: &Panel| ListItem {
                    monogram: Some(panel.monogram()),
                    name: panel.name.clone(),
                    uri: format!("/{}", panel.monogram()),
                    icon: "fa-window-maximize".to_owned(),
                    archived: panel.is_archived,
                    author_html: None,
                    kind: Some(panel_type_name(&panel.panel_type).unwrap_or("Unknown Panel Type")),
                })
                .collect();
            (items, next)
        }
    };
    let (base, heading) = match kind {
        Kind::Dashboards => ("dashboard", "Dashboards"),
        Kind::Panels => ("dashboard/panel", "Panels"),
    };
    Ok(Page(ListPage {
        shell: Shell::for_user(state.config(), viewer, heading, current(uri)),
        base,
        heading,
        query: key,
        queries,
        items,
        next,
    }))
}

/// The default query of a list, the first builtin one ("Authored"),
/// unless the viewer saved or reordered queries.
async fn default_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    kind: Kind,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<ListPage>, PageError> {
    viewer.require_application(state, APPLICATION).await?;
    if phab_app_harbormaster::web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        kind.engine(),
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    list_page(state, viewer, (kind, "authored"), query, uri).await
}

/// `GET /dashboard/`.
pub(crate) async fn dashboards<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ListPage>, PageError> {
    default_list(
        state.as_ref(),
        &viewer,
        Kind::Dashboards,
        query.as_deref(),
        &uri,
    )
    .await
}

/// `GET /dashboard/query/{key}/`.
pub(crate) async fn dashboard_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ListPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let list = (Kind::Dashboards, key.as_str());
    list_page(state.as_ref(), &viewer, list, query.as_deref(), &uri).await
}

/// `GET /dashboard/panel/`.
pub(crate) async fn panels<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ListPage>, PageError> {
    default_list(
        state.as_ref(),
        &viewer,
        Kind::Panels,
        query.as_deref(),
        &uri,
    )
    .await
}

/// `GET /dashboard/panel/query/{key}/`.
pub(crate) async fn panel_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ListPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let list = (Kind::Panels, key.as_str());
    list_page(state.as_ref(), &viewer, list, query.as_deref(), &uri).await
}

/// A row of "Panel Used By".
pub(crate) struct Usage {
    icon: &'static str,
    kind: &'static str,
    link_html: String,
}

#[derive(Template)]
#[template(path = "dashboard_panel.html")]
pub(crate) struct PanelPage {
    shell: Shell,
    name: String,
    archived: bool,
    panel_html: String,
    usage: Vec<Usage>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

/// `W12`: the panel's ID, or `None` for other monograms.
pub(crate) fn parse_panel_monogram(monogram: &str) -> Option<i64> {
    monogram
        .strip_prefix('W')
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
}

/// `newUsageView()`: the dashboards and tab panels using the panel, which
/// the viewer sees.
async fn usage<S: WebState>(
    state: &S,
    viewer: &Viewer,
    panel: &Panel,
) -> Result<Vec<Usage>, PageError> {
    let database = state.database();
    let users = storage::panel_users(database, &panel.phid).await?;
    let mut rows = Vec::new();
    let dashboards: Vec<Phid> = users
        .iter()
        .filter(|phid| phid.phid_type() == storage::DASHBOARD)
        .cloned()
        .collect();
    let panels: Vec<Phid> = users
        .iter()
        .filter(|phid| phid.phid_type() == storage::PANEL)
        .cloned()
        .collect();
    if dashboards.len() + panels.len() < users.len() {
        return Err(PageError::Fallback("a panel used by other objects"));
    }
    let config = state.config();
    let now = state.now();
    let dashboards = storage::load_dashboards(database, &dashboards).await?;
    let dashboards = storage::visible(database, config, viewer, dashboards, now)
        .await
        .map_err(internal)?;
    let panels = storage::load_panels(database, &panels).await?;
    let panels = storage::visible(database, config, viewer, panels, now)
        .await
        .map_err(internal)?;
    for phid in &users {
        if let Some(dashboard) = dashboards.iter().find(|dashboard| dashboard.phid == *phid) {
            rows.push(Usage {
                icon: "fa-dashboard",
                kind: "Dashboard",
                link_html: harbormaster::link_html(
                    &dashboard.name,
                    Some(&dashboard.uri()),
                    dashboard.is_archived(),
                ),
            });
        } else if let Some(user) = panels.iter().find(|user| user.phid == *phid) {
            rows.push(Usage {
                icon: "fa-window-maximize",
                kind: "Panel",
                link_html: harbormaster::link_html(
                    &format!("{} {}", user.monogram(), user.name),
                    Some(&format!("/{}", user.monogram())),
                    user.is_archived,
                ),
            });
        } else {
            // A handle the viewer can't see: PHP shows "Restricted".
            return Err(PageError::Fallback(
                "a panel used by objects the viewer can't see",
            ));
        }
    }
    Ok(rows)
}

/// A quoted value of a title (`renderValue()`).
fn quoted(value: &Value) -> Result<String, PageError> {
    match value {
        Value::String(text) => Ok(format!("\"{}\"", escape(text))),
        _ => Err(PageError::Fallback("a panel transaction not ported")),
    }
}

/// The titles of panel transactions, and the core ones panels have when
/// they are created, which are hidden (`shouldHide()`).
fn title(transaction: &Transaction, author: &str) -> Result<Option<String>, PageError> {
    let is_create = transaction.metadata_flag("core.create");
    let old_is_empty = match &transaction.old_value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Number(number) => number.as_i64() == Some(0),
        Value::Bool(_) => false,
    };
    let kind = transaction.transaction_type.as_str();
    let is_policy = matches!(kind, "core:view-policy" | "core:edit-policy");
    if is_create && transaction.metadata_flag("core.default") {
        return Ok(None);
    }
    if is_create && !is_policy && kind != "core:create" && old_is_empty {
        return Ok(None);
    }
    if is_policy && !is_create && transaction.old_value.is_null() {
        return Ok(None);
    }
    Ok(Some(match kind {
        "core:create" => format!("{author} created this object."),
        "dashpanel:name" => format!(
            "{author} renamed this panel from {} to {}.",
            quoted(&transaction.old_value)?,
            quoted(&transaction.new_value)?
        ),
        "dashpanel:archive" if phab_core::php::is_truthy(&transaction.new_value) => {
            format!("{author} archived this panel.")
        }
        "dashpanel:archive" => format!("{author} activated this panel."),
        _ => return Err(PageError::Fallback("a panel transaction not ported")),
    }))
}

/// The timeline of a panel: the transactions the viewer may see.
async fn timeline<S: WebState>(
    state: &S,
    viewer: &WebUser,
    panel: &Panel,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let transactions = storage::object_transactions(
        database,
        ("dashboard", "dashboard_paneltransaction"),
        &panel.phid,
    )
    .await?;
    let subjects: Vec<_> = transactions.iter().map(Transaction::subject).collect();
    let visible = differential::visible_subjects(
        database,
        state.config(),
        &viewer.user.viewer(),
        &subjects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let shown: Vec<&Transaction> = transactions
        .iter()
        .zip(visible)
        .filter_map(|(transaction, visible)| visible.then_some(transaction))
        .collect();
    if shown.len() > 100 {
        return Err(PageError::Fallback("a long panel timeline"));
    }
    if shown
        .iter()
        .any(|transaction| transaction.comment_phid.is_some())
    {
        return Err(PageError::Fallback("a comment in a panel timeline"));
    }
    let authors: Vec<Phid> = shown
        .iter()
        .map(|transaction| transaction.author_phid.clone())
        .collect();
    if authors
        .iter()
        .any(|author| author.phid_type() != PhidType::USER)
    {
        return Err(PageError::Fallback("a transaction by an application"));
    }
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &authors).await?;
    let mut entries = Vec::new();
    for transaction in shown {
        let author = harbormaster::handle(&handles, &transaction.author_phid).short_link_html();
        if let Some(title_html) = title(transaction, &author)? {
            entries.push(Entry {
                title_html,
                date: time::format_utc(transaction.date_created),
                comment_html: None,
            });
        }
    }
    Ok(entries)
}

/// `GET /W{id}`.
pub(crate) async fn panel<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    id: i64,
    uri: Uri,
) -> Result<Page<PanelPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let state = state.as_ref();
    let as_viewer = viewer.user.viewer();
    let panel = storage::load_panels_by_ids(state.database(), &[id])
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    let panel = storage::visible(
        state.database(),
        state.config(),
        &as_viewer,
        vec![panel],
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop()
    // The query's executeOne() finds nothing: PHP answers 404.
    .ok_or(PageError::NotFound)?;
    let can_edit = storage::can_edit(
        state.database(),
        state.config(),
        &as_viewer,
        &panel,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let panels = Panels::load(state, &as_viewer, std::slice::from_ref(&panel.phid)).await?;
    let renderer = PanelRenderer {
        state,
        viewer: &as_viewer,
    };
    let panel_html = renderer
        .render(
            panels.visible.get(&panel.phid),
            true,
            HeaderMode::Edit,
            None,
            &[],
        )
        .await?;
    let usage = usage(state, &as_viewer, &panel).await?;
    let timeline = timeline(state, &viewer, &panel).await?;
    let (archive_name, archive_icon) = if panel.is_archived {
        ("Activate Panel", "fa-check")
    } else {
        ("Archive Panel", "fa-ban")
    };
    let actions = vec![
        Action {
            name: "Edit Panel",
            icon: "fa-pencil",
            uri: format!("/dashboard/panel/edit/{id}/"),
            enabled: can_edit,
        },
        Action {
            name: archive_name,
            icon: archive_icon,
            uri: format!("/dashboard/panel/archive/{id}/"),
            enabled: can_edit,
        },
    ];
    let title = format!("{} {}", panel.monogram(), panel.name);
    Ok(Page(PanelPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
        name: panel.name.clone(),
        archived: panel.is_archived,
        panel_html,
        usage,
        timeline,
        actions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_monograms_are_digits() {
        assert_eq!(parse_panel_monogram("W12"), Some(12));
        assert_eq!(parse_panel_monogram("W"), None);
        assert_eq!(parse_panel_monogram("W1x"), None);
        assert_eq!(parse_panel_monogram("D1"), None);
    }
}
