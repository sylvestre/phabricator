//! The dashboard console (`/dashboard/console/`,
//! `PhabricatorDashboardConsoleController`) and the portal list
//! (`/portal/`, `/portal/query/all/`: `PhabricatorDashboardPortalListController`
//! with its only builtin query). Saved queries and search parameters go
//! to PHP.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_dashboard::storage::{self, Portal};

use super::APPLICATION;
use super::lists::visible_page;
use crate::diff_html::escape;
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::system::{Body, Item, Link, List, PageBuilder, Section, SystemPage};
use crate::viewer::WebUser;

/// `PhabricatorDashboardPortalSearchEngine`.
const PORTAL_ENGINE: &str = "PhabricatorDashboardPortalSearchEngine";

/// `GET /dashboard/console/`.
pub(crate) async fn console<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    viewer.require_application(state, APPLICATION).await?;
    let item = |name: &str, uri: &str, icon: &str, text: &str| Item {
        name: name.to_owned(),
        uri: Some(uri.to_owned()),
        icon: Some(icon.to_owned()),
        attributes_html: vec![escape(text)],
        ..Item::default()
    };
    let items = vec![
        item(
            "Portals",
            "/portal/",
            "fa-compass",
            "Portals are collections of dashboards, links, and other resources that can \
             provide a high-level overview of a project.",
        ),
        item(
            "Dashboards",
            "/dashboard/",
            "fa-dashboard",
            "Dashboards organize panels, creating a cohesive page for analysis or action.",
        ),
        item(
            "Panels",
            "/dashboard/panel/",
            "fa-line-chart",
            "Panels show queries, charts, and other information to provide insight on a \
             particular topic.",
        ),
    ];
    Ok(PageBuilder::new("Dashboard Console")
        .section(Section::untitled(Body::List(List {
            items,
            empty: String::new(),
        })))
        .build(state, &viewer, &uri))
}

async fn portal_page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<SystemPage>, PageError> {
    let after = crate::herald::cursor(query)?;
    let database = state.database();
    let id: fn(&Portal) -> i64 = |portal| portal.id;
    let (portals, next) = visible_page(
        state,
        &viewer.user.viewer(),
        (after, id),
        |before, limit| storage::portal_page(database, (before, limit)),
    )
    .await?;
    let items = portals
        .iter()
        .map(|portal| Item {
            name: portal.name.clone(),
            uri: Some(portal.uri()),
            // `setObjectName()`.
            attributes_html: vec![escape(&format!("Portal {}", portal.id))],
            ..Item::default()
        })
        .collect();
    let mut page = PageBuilder::new("Portals")
        .icon("fa-compass")
        .nav(vec![
            Link::new("All Portals", "/portal/query/all/").inactive(true),
        ])
        .section(Section::untitled(Body::List(List {
            items,
            empty: "No portals found.".to_owned(),
        })))
        .side(Link::new("Create Portal", "/portal/edit/").icon("fa-plus-square"));
    if let Some(next) = next {
        page = page.section(Section::untitled(Body::Html(format!(
            "<nav class=\"pager\"><a href=\"/portal/query/all/?after={next}\">Next Page</a></nav>"
        ))));
    }
    Ok(page.build(state, viewer, uri))
}

/// `GET /portal/`: the default query, "All Portals", unless the viewer
/// saved or reordered queries.
pub(crate) async fn portals<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    viewer.require_application(state, APPLICATION).await?;
    if phab_app_harbormaster::web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        PORTAL_ENGINE,
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    portal_page(state, &viewer, query.as_deref(), &uri).await
}

/// `GET /portal/query/{key}/`.
pub(crate) async fn portal_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    viewer.require_application(state, APPLICATION).await?;
    if key != "all" {
        return Err(PageError::Fallback("a saved search query"));
    }
    portal_page(state, &viewer, query.as_deref(), &uri).await
}
