//! `/portal/view/<id>/` (`PhabricatorDashboardPortalViewController`): the
//! portal's menu (`PhabricatorDashboardPortalProfileMenuEngine`) and the
//! content of its default item, a dashboard. The other item routes
//! (`view/<item>/`, `configure/`...) are PHP's.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_dashboard::menu::{self, MenuItem};
use phab_app_dashboard::storage::{self, Dashboard};
use phab_core::Phid;

use super::{APPLICATION, ColumnView, internal, parse_id, render_dashboard};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// An entry of the portal's menu (`PhabricatorProfileMenuItemView`).
pub(crate) enum MenuView {
    Divider,
    Label(String),
    Link {
        name: String,
        uri: String,
        icon: String,
        tooltip: Option<String>,
        disabled: bool,
        selected: bool,
    },
}

/// What the portal shows next to its menu.
pub(crate) enum Content {
    /// A dashboard.
    Dashboard(Vec<ColumnView>),
    /// An empty state: its title and message.
    Empty(&'static str, &'static str),
}

#[derive(Template)]
#[template(path = "portal.html")]
pub(crate) struct PortalPage {
    shell: Shell,
    name: String,
    archived: bool,
    heading: String,
    menu: Vec<MenuView>,
    content: Content,
}

/// `PhabricatorProfileMenuItemIconSet`.
fn link_icon(key: &str) -> &'static str {
    match key {
        "maniphest" => "fa-anchor",
        "feed" => "fa-newspaper-o",
        "phriction" => "fa-book",
        "conpherence" => "fa-comments",
        "differential" => "fa-cog",
        "diffusion" => "fa-code",
        "calendar" => "fa-calendar",
        "create" => "fa-plus",
        _ => "fa-link",
    }
}

/// Whether a link item's URI is one PHP accepts as a link
/// (`isValidURIForLink()`), so it can go in an `href`.
fn is_link_uri(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || (uri.starts_with('/') && !uri.starts_with("//"))
}

/// A dashboard item and its dashboard, if the viewer can see it.
struct DashboardItem<'a> {
    item: &'a MenuItem,
    dashboard: Option<&'a Dashboard>,
    restricted: bool,
}

/// The dashboard of a dashboard item.
fn dashboard_entry<'a>(
    item: &'a MenuItem,
    dashboards: &'a HashMap<Phid, Dashboard>,
    existing: &HashSet<Phid>,
) -> DashboardItem<'a> {
    let phid = item.property("dashboardPHID").map(Phid::new);
    DashboardItem {
        item,
        dashboard: phid.as_ref().and_then(|phid| dashboards.get(phid)),
        restricted: phid.as_ref().is_some_and(|phid| existing.contains(phid)),
    }
}

impl DashboardItem<'_> {
    /// `getDisplayName()`.
    fn name(&self) -> String {
        match self.dashboard {
            None if self.restricted => "Restricted Dashboard".to_owned(),
            None => "Invalid Dashboard".to_owned(),
            Some(dashboard) if dashboard.is_archived() => "Archived Dashboard".to_owned(),
            Some(dashboard) => self
                .item
                .property("name")
                .filter(|name| !name.is_empty())
                .map_or_else(|| dashboard.name.clone(), str::to_owned),
        }
    }

    /// Items of dashboards which can't be shown are disabled.
    fn is_disabled(&self) -> bool {
        self.dashboard.is_none_or(Dashboard::is_archived)
    }
}

/// `GET /portal/view/{id}/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<PortalPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let id = parse_id(&id)?;
    let state = state.as_ref();
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    let portal = storage::load_portals_by_ids(database, &[id])
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    let portal = storage::visible(
        database,
        state.config(),
        &as_viewer,
        vec![portal],
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop()
    .ok_or(PageError::PolicyDenied)?;

    let items = menu::portal_items(database, &portal.phid).await?;
    if items.iter().any(|item| {
        !matches!(
            item.menu_item_key.as_str(),
            menu::DIVIDER
                | menu::LABEL
                | menu::LINK
                | menu::DASHBOARD
                | menu::MANAGE_MENU
                | menu::MANAGE_PORTAL
        )
    }) {
        return Err(PageError::Fallback("a portal menu item not ported"));
    }
    // Disabled items build no views.
    let items: Vec<&MenuItem> = items.iter().filter(|item| !item.is_disabled()).collect();

    // `willGetMenuItemViewList()` of the dashboard items.
    let dashboard_phids: Vec<Phid> = items
        .iter()
        .filter(|item| item.menu_item_key == menu::DASHBOARD)
        .filter_map(|item| item.property("dashboardPHID"))
        .map(Phid::new)
        .collect();
    let dashboards = storage::load_dashboards(database, &dashboard_phids).await?;
    let existing: HashSet<Phid> = dashboards
        .iter()
        .map(|dashboard| dashboard.phid.clone())
        .collect();
    let dashboards: HashMap<Phid, Dashboard> = storage::visible(
        database,
        state.config(),
        &as_viewer,
        dashboards,
        state.now(),
    )
    .await
    .map_err(internal)?
    .into_iter()
    .map(|dashboard| (dashboard.phid.clone(), dashboard))
    .collect();
    let entry = |item| dashboard_entry(item, &dashboards, &existing);

    // `getDefaultViews()`: the dashboards which can be shown, the pinned
    // ones first.
    let candidates: Vec<&MenuItem> = items
        .iter()
        .copied()
        .filter(|item| item.menu_item_key == menu::DASHBOARD && !entry(item).is_disabled())
        .collect();
    let selected = candidates
        .iter()
        .find(|item| item.is_default())
        .or_else(|| candidates.first())
        .copied();

    let base = portal.uri();
    let mut views = Vec::new();
    for item in &items {
        let is_selected = selected.is_some_and(|selected| std::ptr::eq(selected, *item));
        views.push(match item.menu_item_key.as_str() {
            menu::DIVIDER => MenuView::Divider,
            menu::LABEL => MenuView::Label(item.property("name").unwrap_or("").to_owned()),
            menu::LINK => {
                let uri = item.property("uri").unwrap_or("");
                if !is_link_uri(uri) {
                    return Err(PageError::Fallback("an unusual portal link"));
                }
                MenuView::Link {
                    name: item.property("name").unwrap_or("").to_owned(),
                    uri: uri.to_owned(),
                    icon: link_icon(item.property("icon").unwrap_or("link")).to_owned(),
                    tooltip: item
                        .property("tooltip")
                        .filter(|tooltip| !tooltip.is_empty())
                        .map(str::to_owned),
                    disabled: false,
                    selected: false,
                }
            }
            menu::DASHBOARD => {
                let entry = entry(item);
                let icon = match entry.dashboard {
                    Some(dashboard) if !dashboard.is_archived() => {
                        dashboard_icon(&dashboard.icon).to_owned()
                    }
                    _ => "fa-ban".to_owned(),
                };
                MenuView::Link {
                    name: entry.name(),
                    uri: format!("{base}view/{}/", item.identifier()),
                    icon,
                    tooltip: None,
                    disabled: entry.is_disabled(),
                    selected: is_selected,
                }
            }
            menu::MANAGE_MENU => MenuView::Link {
                name: item
                    .property("name")
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Edit Menu")
                    .to_owned(),
                uri: format!("{base}configure/"),
                icon: "fa-pencil".to_owned(),
                tooltip: None,
                disabled: false,
                selected: false,
            },
            _ => MenuView::Link {
                name: item
                    .property("name")
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Manage Portal")
                    .to_owned(),
                uri: format!("{base}view/{}/", item.identifier()),
                icon: "fa-pencil".to_owned(),
                tooltip: None,
                disabled: false,
                selected: false,
            },
        });
    }

    let (heading, content) = match selected {
        Some(item) => {
            let entry = entry(item);
            // Selected items have a dashboard which isn't archived.
            let dashboard = entry
                .dashboard
                .ok_or(PageError::Fallback("an invalid portal dashboard"))?;
            (
                entry.name(),
                Content::Dashboard(render_dashboard(state, &as_viewer, dashboard, false).await?),
            )
        }
        None => (
            "Empty".to_owned(),
            Content::Empty("No Content", "No visible menu items can render content."),
        ),
    };

    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(PortalPage {
        shell: Shell::for_user(state.config(), &viewer, &heading, current),
        name: portal.name.clone(),
        archived: portal.status == "archived",
        heading,
        menu: views,
        content,
    }))
}

/// A dashboard's icon as a class: only Font Awesome names.
fn dashboard_icon(icon: &str) -> &str {
    if icon.starts_with("fa-")
        && icon
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        icon
    } else {
        "fa-dashboard"
    }
}
