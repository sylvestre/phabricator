//! The "configure" route of the profile menus
//! (`PhabricatorProfileMenuEngine::buildMenuEditModeContent()`): the
//! portal menus (`/portal/view/<id>/configure/`), the home menu
//! (`/home/menu/configure/`) and the favorites menu
//! (`/favorites/menu/configure/`). With one edit mode the viewer may use,
//! it redirects to that mode's page; with both, it lists them. The pages
//! of the modes, which list and edit the items, stay with PHP.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_app_dashboard::storage;
use phab_core::Config;
use phab_policy::POLICY_ADMIN;

use super::{APPLICATION, internal, parse_id};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::favorites;
use crate::state::WebState;
use crate::system::{Body, Item, List, PageBuilder, Section};
use crate::viewer::WebUser;

/// `AphrontRedirectResponse` to a local path.
fn redirect(config: &Config, path: &str) -> Response {
    let location = config.uri(path).unwrap_or_else(|_| path.to_owned());
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /portal/view/{id}/configure/`: portals aren't personalizable, so
/// editors go to the global items and other viewers get a 404.
pub(crate) async fn portal<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    viewer.require_application(state, APPLICATION).await?;
    let id = parse_id(&id)?;
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
    let can_edit = storage::can_edit(database, state.config(), &as_viewer, &portal, state.now())
        .await
        .map_err(internal)?;
    if !can_edit {
        return Err(PageError::NotFound);
    }
    Ok(redirect(
        state.config(),
        &format!("{}configure/global/", portal.uri()),
    ))
}

/// The configure route of a personalizable menu on the application
/// `class`, at `base`: personal items for everyone, global items for
/// viewers who can edit the application.
async fn application_menu<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (class, base): (&'static str, &str),
    uri: &Uri,
) -> Result<Response, PageError> {
    viewer.require_application(state, class).await?;
    let can_edit = favorites::has_application_capability(
        state,
        &viewer.user.viewer(),
        class,
        ("edit", POLICY_ADMIN),
    )
    .await?;
    if !can_edit {
        return Ok(redirect(
            state.config(),
            &format!("{base}configure/custom/"),
        ));
    }
    let item = |name: &str, path: &str, icon: &str, text: &str| Item {
        name: name.to_owned(),
        uri: Some(format!("{base}{path}")),
        icon: Some(icon.to_owned()),
        attributes_html: vec![escape(text)],
        ..Item::default()
    };
    let items = vec![
        item(
            "Personal Menu Items",
            "configure/custom/",
            "fa-user",
            "Edit the menu for your personal account.",
        ),
        item(
            "Global Menu Items",
            "configure/global/",
            "fa-globe",
            "Edit the global default menu for all users.",
        ),
    ];
    Ok(PageBuilder::new("Manage Menu")
        .title("Configure Menu")
        .icon("fa-list")
        .section(Section::untitled(Body::List(List {
            items,
            empty: String::new(),
        })))
        .build(state, viewer, uri)
        .into_response())
}

/// `GET /home/menu/configure/`.
pub(crate) async fn home<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Response, PageError> {
    let menu = ("PhabricatorHomeApplication", "/home/menu/");
    application_menu(state.as_ref(), &viewer, menu, &uri).await
}

/// `GET /favorites/menu/configure/`.
pub(crate) async fn favorites<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Response, PageError> {
    let menu = ("PhabricatorFavoritesApplication", "/favorites/menu/");
    application_menu(state.as_ref(), &viewer, menu, &uri).await
}
