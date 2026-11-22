//! `/dashboard/view/<id>/` (`PhabricatorDashboardViewController`): the
//! dashboard with its panels, the menus it is in and its timeline.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_dashboard::menu;
use phab_app_dashboard::storage::{self, Dashboard, Transaction};
use phab_app_differential::storage as differential;
use phab_app_project as project;
use phab_core::{Phid, PhidType};
use phab_policy::Viewer;
use serde_json::Value;

use super::{APPLICATION, ColumnView, internal, parse_id, render_dashboard};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Action, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::timeline::Entry;
use crate::viewer::WebUser;

/// A row of "Dashboard Used By".
pub(crate) struct Usage {
    icon: String,
    kind: &'static str,
    menu_html: String,
    owner_html: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
pub(crate) struct DashboardPage {
    shell: Shell,
    name: String,
    icon: String,
    archived: bool,
    columns: Vec<ColumnView>,
    usage: Vec<Usage>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

/// The dashboard with this ID, if the viewer can see it.
async fn visible_dashboard<S: WebState>(
    state: &S,
    viewer: &Viewer,
    id: i64,
) -> Result<Dashboard, PageError> {
    let dashboard = storage::load_dashboards_by_ids(state.database(), &[id])
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    storage::visible(
        state.database(),
        state.config(),
        viewer,
        vec![dashboard],
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop()
    .ok_or(PageError::PolicyDenied)
}

/// `newUsageView()`: the portal and project menus with the dashboard,
/// globally or for the viewer. Other menus (home, favorites, profiles)
/// are PHP's.
async fn usage<S: WebState>(
    state: &S,
    viewer: &WebUser,
    dashboard: &Dashboard,
) -> Result<Vec<Usage>, PageError> {
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    let mut items =
        menu::items_affecting(database, &dashboard.phid, Some(&viewer.user.phid)).await?;
    items.retain(|item| item.menu_item_key == menu::DASHBOARD);
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let portal_phids: Vec<Phid> = items
        .iter()
        .map(|item| item.profile_phid.clone())
        .filter(|phid| phid.phid_type() == storage::PORTAL)
        .collect();
    let project_phids: Vec<Phid> = items
        .iter()
        .map(|item| item.profile_phid.clone())
        .filter(|phid| phid.phid_type() == PhidType::PROJECT)
        .collect();
    if portal_phids.len() + project_phids.len() < items.len() {
        return Err(PageError::Fallback("a dashboard in other menus"));
    }

    // (icon, type, link) of the menus the viewer can see.
    let mut menus: std::collections::HashMap<Phid, (String, &'static str, String)> =
        std::collections::HashMap::new();
    let portals = storage::load_portals(database, &portal_phids).await?;
    for portal in storage::visible(database, state.config(), &as_viewer, portals, state.now())
        .await
        .map_err(internal)?
    {
        menus.insert(
            portal.phid.clone(),
            (
                "fa-compass".to_owned(),
                "Portal Menu",
                format!(
                    "<a href=\"{}\">{}</a>",
                    escape(&portal.uri()),
                    escape(&portal.name)
                ),
            ),
        );
    }
    let projects = project::storage::load_by_phid(database, &project_phids).await?;
    let (projects, graph) = project::policy::visible_projects(
        database,
        state.config(),
        &as_viewer,
        projects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    for project in &projects {
        menus.insert(
            project.phid.clone(),
            (
                project.icon.clone(),
                "Project Menu",
                harbormaster::link_html(
                    &project.display_name(graph.parent(project)),
                    Some(&project.uri()),
                    project.is_archived(),
                ),
            ),
        );
    }

    let owners: Vec<Phid> = items
        .iter()
        .filter_map(|item| item.custom_phid.clone())
        .collect();
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &owners).await?;

    // `newUsageSortVector()`: global, then personal, by ID.
    items.sort_by_key(|item| (item.custom_phid.is_some(), item.id));
    Ok(items
        .iter()
        // The query drops the items of menus the viewer can't see.
        .filter_map(|item| {
            let (icon, kind, menu_html) = menus.get(&item.profile_phid)?.clone();
            let owner_html = item.custom_phid.as_ref().map_or_else(
                || "Global".to_owned(),
                |owner| harbormaster::handle(&handles, owner).link_html(),
            );
            Some(Usage {
                icon,
                kind,
                menu_html,
                owner_html,
            })
        })
        .collect())
}

/// A quoted value of a title (`renderValue()`).
fn quoted(value: &Value) -> Result<String, PageError> {
    match value {
        Value::String(text) => Ok(format!("\"{}\"", escape(text))),
        _ => Err(PageError::Fallback("a dashboard transaction not ported")),
    }
}

/// The titles of `PhabricatorDashboardTransaction`, and the core ones
/// dashboards have when they are created (`shouldHide()`).
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
    let old = || quoted(&transaction.old_value);
    let new = || quoted(&transaction.new_value);
    Ok(Some(match kind {
        "core:create" => format!("{author} created this object."),
        "dashboard:name" => format!(
            "{author} renamed this dashboard from {} to {}.",
            old()?,
            new()?
        ),
        "dashboard:icon" => format!(
            "{author} changed the icon for this dashboard from {} to {}.",
            old()?,
            new()?
        ),
        "dashboard:layoutmode" => format!(
            "{author} changed the layout mode for this dashboard from {} to {}.",
            old()?,
            new()?
        ),
        "panels" => format!("{author} changed the panels on this dashboard."),
        "dashboard:status" => match transaction.new_value.as_str() {
            Some("active") => format!("{author} activated this dashboard."),
            Some("archived") => format!("{author} archived this dashboard."),
            _ => return Ok(None),
        },
        _ => return Err(PageError::Fallback("a dashboard transaction not ported")),
    }))
}

/// The timeline of a dashboard: the transactions the viewer may see.
async fn timeline<S: WebState>(
    state: &S,
    viewer: &WebUser,
    dashboard: &Dashboard,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let transactions = storage::dashboard_transactions(database, &dashboard.phid).await?;
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
    // The timeline pages its transactions, and comments are not ported.
    if shown.len() > 100 {
        return Err(PageError::Fallback("a long dashboard timeline"));
    }
    if shown
        .iter()
        .any(|transaction| transaction.comment_phid.is_some())
    {
        return Err(PageError::Fallback("a comment in a dashboard timeline"));
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

/// `GET /dashboard/view/{id}/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<DashboardPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let id = parse_id(&id)?;
    let as_viewer = viewer.user.viewer();
    let state = state.as_ref();
    let dashboard = visible_dashboard(state, &as_viewer, id).await?;
    let can_edit = storage::can_edit(
        state.database(),
        state.config(),
        &as_viewer,
        &dashboard,
        state.now(),
    )
    .await
    .map_err(internal)?;

    let columns = render_dashboard(state, &as_viewer, &dashboard, can_edit).await?;
    let usage = usage(state, &viewer, &dashboard).await?;
    let timeline = timeline(state, &viewer, &dashboard).await?;

    let mut actions = vec![
        Action {
            name: "Edit Dashboard",
            icon: "fa-pencil",
            uri: format!("/dashboard/edit/{id}/"),
            enabled: can_edit,
        },
        Action {
            name: "Add Dashboard to Menu",
            icon: "fa-wrench",
            uri: format!("/dashboard/install/{id}/"),
            enabled: true,
        },
    ];
    actions.push(if dashboard.is_archived() {
        Action {
            name: "Activate Dashboard",
            icon: "fa-check",
            uri: format!("/dashboard/archive/{id}/"),
            enabled: can_edit,
        }
    } else {
        Action {
            name: "Archive Dashboard",
            icon: "fa-ban",
            uri: format!("/dashboard/archive/{id}/"),
            enabled: can_edit,
        }
    });

    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(DashboardPage {
        shell: Shell::for_user(state.config(), &viewer, &dashboard.name, current),
        archived: dashboard.is_archived(),
        icon: dashboard.icon.clone(),
        name: dashboard.name.clone(),
        columns,
        usage,
        timeline,
        actions,
    }))
}
