//! The dashboard writes of the web interface, made as PHP's make them:
//! the edit forms of panels (`/dashboard/panel/edit/<id>/`,
//! `PhabricatorDashboardPanelEditEngine`) and portals
//! (`/portal/edit/<id>/`, `PhabricatorDashboardPortalEditEngine`), and
//! archiving or activating panels (`/dashboard/panel/archive/<id>/`) and
//! dashboards (`/dashboard/archive/<id>/`). They run the Rust dashboard
//! editor ([`phab_app_dashboard::edit`]) with a web content source; their
//! GET pages are the forms and dialogs PHP shows.
//!
//! The edit forms post PHP's fields (`name`, a text panel's `text`,
//! `policy.view`, `policy.edit`, a portal's `projectPHIDs` with their
//! `.initial` values), so the replay of a post Rust leaves to PHP is PHP's
//! own form post. Rust writes the name; PHP's other transactions have no
//! effect when their values didn't change, and are dropped as
//! `setContinueOnNoEffect()` does. Left to PHP (`421`, before writing):
//! changes to the text, the policies or the tags, a missing field, forms
//! opened from a dashboard or a tab panel (`?contextPHID=`) and the forms
//! of new panels and portals, query panels and panel types not ported,
//! workflow (Ajax) requests, invalid CSRF tokens, objects the viewer can't
//! edit, and every edit the editor leaves to PHP, which includes the
//! validation errors PHP shows on the form and edits without effect.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing};
use phab_app_dashboard::edit::{self, Change, Target};
use phab_app_dashboard::storage::{self, Dashboard, Panel, Policied, Portal};
use phab_conduit::MethodError;
use phab_conduit::edit::Identifier;
use phab_core::Phid;
use phab_db::editor::WEB_SOURCE;
use serde_json::Map;

use super::{APPLICATION, internal, parse_id};
use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::login::request::LoginRequest;
use crate::project_edit::{self, PolicyField};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// `PhabricatorCookies::COOKIE_SUBMIT`.
const SUBMIT_COOKIE: &str = "phfrm";

/// The tags of a portal, which the Rust form posts unchanged.
pub(crate) struct Tags {
    names: Vec<String>,
    phids: Vec<String>,
}

#[derive(Template)]
#[template(path = "dashboard_edit.html")]
pub(crate) struct EditPage {
    shell: Shell,
    title: String,
    action: String,
    name: String,
    /// A text panel's text.
    text: Option<String>,
    policies: Vec<PolicyField>,
    tags: Option<Tags>,
    cancel: String,
}

fn method_error(error: MethodError) -> PageError {
    match error {
        MethodError::Unsupported(reason) => PageError::Fallback(reason),
        // PHP explains its refusals on the form.
        MethodError::Conduit { .. } => PageError::Fallback("edits PHP refuses"),
        MethodError::Internal(error) => PageError::Internal(Box::new(std::io::Error::other(error))),
    }
}

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// Runs the editor on object `id` of `target` as the viewer.
async fn apply<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (target, id): (Target, i64),
    change: Change,
) -> Result<(), PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let context = web_edit::context(state);
    let request = web_edit::request(state, &context, viewer, Map::new());
    edit::apply(&request, target, Identifier::Id(id), change, WEB_SOURCE)
        .await
        .map_err(method_error)?;
    Ok(())
}

/// The object of `rows` the viewer sees, and whether they can edit it.
async fn visible<S: WebState, T: Policied>(
    state: &S,
    viewer: &WebUser,
    rows: Vec<T>,
) -> Result<Option<(T, bool)>, PageError> {
    viewer.require_application(state, APPLICATION).await?;
    let as_viewer = viewer.user.viewer();
    let database = state.database();
    let Some(row) = storage::visible(database, state.config(), &as_viewer, rows, state.now())
        .await
        .map_err(internal)?
        .pop()
    else {
        return Ok(None);
    };
    let can_edit = storage::can_edit(database, state.config(), &as_viewer, &row, state.now())
        .await
        .map_err(internal)?;
    Ok(Some((row, can_edit)))
}

/// The object an edit form edits: PHP answers 404 for objects the viewer
/// can't see and explains its policy for those they can't edit.
async fn editable<S: WebState, T: Policied>(
    state: &S,
    viewer: &WebUser,
    rows: Vec<T>,
    uri: &Uri,
) -> Result<T, PageError> {
    // `?contextPHID=` and the like change where the form goes when done.
    if uri.query().is_some_and(|query| !query.is_empty()) {
        return Err(PageError::Fallback("edit forms with parameters"));
    }
    let (row, can_edit) = visible(state, viewer, rows)
        .await?
        .ok_or(PageError::NotFound)?;
    if !can_edit {
        return Err(PageError::Fallback("objects the viewer can't edit"));
    }
    Ok(row)
}

/// A field of the post, which PHP's form always has.
fn field(request: &LoginRequest, key: &str) -> Result<String, PageError> {
    request
        .str(key)
        .map(str::to_owned)
        .ok_or(PageError::Fallback("form posts without every field"))
}

/// The common checks of an edit form post: the post itself, the policies
/// (which Rust doesn't change) and the posted name, the only change Rust
/// writes; the editor leaves an unchanged name to PHP.
fn posted_name<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
    (view_policy, edit_policy): (&str, &str),
) -> Result<String, PageError> {
    web_edit::check_form_post(state, viewer, request)?;
    if !request.bool("editEngine") {
        // PHP shows the form again.
        return Err(PageError::Fallback("posts which aren't edit form posts"));
    }
    let posted = field(request, "name")?;
    if field(request, "policy.view")? != view_policy
        || field(request, "policy.edit")? != edit_policy
    {
        return Err(PageError::Fallback("policy changes"));
    }
    Ok(posted)
}

/// The saved form's redirect (`newEditResponse()`).
fn done<S: WebState>(state: &S, mut request: LoginRequest, engine: &str, uri: &str) -> Response {
    request.set_temporary_cookie(
        state.config(),
        SUBMIT_COOKIE,
        &format!("edit-engine/{engine}"),
        state.now(),
    );
    request.redirect(state.config(), uri)
}

/// The text of a text panel the form edits: panels without one get a
/// text transaction from PHP's form (`null` to `""`).
fn panel_text(panel: &Panel) -> Result<Option<String>, PageError> {
    match panel.panel_type.as_str() {
        "text" => match panel.property("text") {
            Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
            _ => Err(PageError::Fallback("text panels without text")),
        },
        "tabs" => Ok(None),
        "query" => Err(PageError::Fallback("the form of query panels")),
        _ => Err(PageError::Fallback("panel types not ported")),
    }
}

/// `GET|POST /dashboard/panel/edit/{id}/`.
pub(crate) async fn panel_edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let id = parse_id(&id)?;
    let panels = storage::load_panels_by_ids(state.database(), &[id]).await?;
    let panel = editable(state, &viewer, panels, &uri).await?;
    let text = panel_text(&panel)?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if !request.is_post() {
        let title = format!("Edit Panel: {}", panel.name);
        let values = vec![
            ("policy.view", "Visible To", panel.view_policy.clone()),
            ("policy.edit", "Editable By", panel.edit_policy.clone()),
        ];
        return Ok(Page(EditPage {
            shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
            title,
            action: format!("/dashboard/panel/edit/{id}/"),
            name: panel.name.clone(),
            text,
            policies: project_edit::policy_controls(state, &viewer, None, values).await?,
            tags: None,
            cancel: format!("/{}", panel.monogram()),
        })
        .into_response());
    }
    let policies = (panel.view_policy.as_str(), panel.edit_policy.as_str());
    let name = posted_name(state, &viewer, &request, policies)?;
    if let Some(text) = &text
        && field(&request, "text")? != *text
    {
        return Err(PageError::Fallback("text changes"));
    }
    apply(state, &viewer, (Target::Panel, id), Change::Name(name)).await?;
    Ok(done(
        state,
        request,
        "dashboard.panel",
        &format!("/{}", panel.monogram()),
    ))
}

/// `getArr()`, else `getStrList()`, of a tokenizer.
fn token_list(request: &LoginRequest, key: &str) -> Vec<String> {
    let array = request.array(key);
    if !array.is_empty() {
        return array.into_iter().map(|(_, value)| value).collect();
    }
    request
        .str(key)
        .map(|text| {
            text.split(|character: char| character == ',' || character.is_ascii_whitespace())
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The portal's tags, as the form starts with them (`array_reverse()` of
/// the edges, oldest first).
async fn portal_tags<S: WebState>(state: &S, portal: &Portal) -> Result<Vec<Phid>, PageError> {
    let mut projects = storage::projects(state.database(), std::slice::from_ref(&portal.phid))
        .await?
        .remove(&portal.phid)
        .unwrap_or_default();
    projects.reverse();
    Ok(projects)
}

/// `GET|POST /portal/edit/{id}/`.
pub(crate) async fn portal_edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let id = parse_id(&id)?;
    let portals = storage::load_portals_by_ids(state.database(), &[id]).await?;
    let portal = editable(state, &viewer, portals, &uri).await?;
    let tags = portal_tags(state, &portal).await?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let manage = format!("/portal/view/{id}/view/manage/");
    if !request.is_post() {
        let title = format!("Edit Portal: {}", portal.name);
        let values = vec![
            ("policy.view", "Visible To", portal.view_policy.clone()),
            ("policy.edit", "Editable By", portal.edit_policy.clone()),
        ];
        let scope = Scope::new(state, &viewer);
        let handles = harbormaster::load_handles(state, &scope, &tags).await?;
        let names = tags
            .iter()
            .map(|phid| harbormaster::handle(&handles, phid).name.clone())
            .collect();
        return Ok(Page(EditPage {
            shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
            title,
            action: format!("/portal/edit/{id}/"),
            name: portal.name.clone(),
            text: None,
            policies: project_edit::policy_controls(state, &viewer, None, values).await?,
            tags: Some(Tags {
                names,
                phids: tags.iter().map(ToString::to_string).collect(),
            }),
            cancel: manage,
        })
        .into_response());
    }
    let policies = (portal.view_policy.as_str(), portal.edit_policy.as_str());
    let name = posted_name(state, &viewer, &request, policies)?;
    let tags: Vec<String> = tags.iter().map(ToString::to_string).collect();
    let initial: Vec<String> = request
        .array("projectPHIDs.initial")
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    if token_list(&request, "projectPHIDs") != tags || initial != tags {
        return Err(PageError::Fallback("tag changes"));
    }
    apply(state, &viewer, (Target::Portal, id), Change::Name(name)).await?;
    Ok(done(state, request, "portal", &manage))
}

/// The object an archive action changes: PHP's query requires
/// `CAN_VIEW` and `CAN_EDIT`. It answers 404 for objects the viewer can't
/// see; for those it can see but not edit, the policy exception is PHP's
/// "You Shall Not Pass" dialog, which is left to PHP.
async fn archivable<S: WebState, T: Policied>(
    state: &S,
    viewer: &WebUser,
    rows: Vec<T>,
) -> Result<T, PageError> {
    match visible(state, viewer, rows).await? {
        Some((row, true)) => Ok(row),
        Some((_, false)) => Err(PageError::Fallback("objects the viewer can't edit")),
        None => Err(PageError::NotFound),
    }
}

/// `GET|POST /dashboard/panel/archive/{id}/`
/// (`PhabricatorDashboardPanelArchiveController`).
pub(crate) async fn panel_archive<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let id = parse_id(&id)?;
    let panels = storage::load_panels_by_ids(state.database(), &[id]).await?;
    let panel = archivable(state, &viewer, panels).await?;
    let next = format!("/{}", panel.monogram());
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if request.is_post() {
        web_edit::check_form_post(state, &viewer, &request)?;
        let change = Change::PanelArchived(!panel.is_archived);
        apply(state, &viewer, (Target::Panel, id), change).await?;
        return Ok(request.redirect(state.config(), &next));
    }
    let content = if panel.is_archived {
        (
            "Activate Panel?",
            vec![
                "This panel will be reactivated and appear in other interfaces as an active \
                 panel.",
            ],
            Some("Activate Panel"),
        )
    } else {
        (
            "Archive Panel?",
            vec!["This panel will be archived and no longer appear in lists of active panels."],
            Some("Archive Panel"),
        )
    };
    Ok(project_edit::dialog(
        state,
        &viewer,
        &uri,
        content,
        Vec::new(),
        next,
    ))
}

/// `GET|POST /dashboard/archive/{id}/`
/// (`PhabricatorDashboardArchiveController`).
pub(crate) async fn dashboard_archive<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let id = parse_id(&id)?;
    let dashboards = storage::load_dashboards_by_ids(state.database(), &[id]).await?;
    let dashboard: Dashboard = archivable(state, &viewer, dashboards).await?;
    let view = dashboard.uri();
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if request.is_post() {
        web_edit::check_form_post(state, &viewer, &request)?;
        let status = if dashboard.is_archived() {
            "active"
        } else {
            "archived"
        };
        let change = Change::DashboardStatus(status);
        apply(state, &viewer, (Target::Dashboard, id), change).await?;
        return Ok(request.redirect(state.config(), &view));
    }
    let content = if dashboard.is_archived() {
        (
            "Activate Dashboard",
            vec!["This dashboard will become active again."],
            Some("Activate Dashboard"),
        )
    } else {
        (
            "Archive Dashboard",
            vec!["This dashboard will be marked as archived."],
            Some("Archive Dashboard"),
        )
    };
    Ok(project_edit::dialog(
        state,
        &viewer,
        &uri,
        content,
        Vec::new(),
        view,
    ))
}

/// PHP's routes of the dashboard writes.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/dashboard/panel/edit/{id}/",
            routing::get(panel_edit::<S>).post(panel_edit::<S>),
        )
        .route(
            "/portal/edit/{id}/",
            routing::get(portal_edit::<S>).post(portal_edit::<S>),
        )
        .route(
            "/dashboard/panel/archive/{id}/",
            routing::get(panel_archive::<S>).post(panel_archive::<S>),
        )
        .route(
            "/dashboard/archive/{id}/",
            routing::get(dashboard_archive::<S>).post(dashboard_archive::<S>),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_panels_need_their_text() {
        let mut panel = Panel {
            id: 1,
            phid: Phid::new("PHID-DSHP-1"),
            name: "Notes".to_owned(),
            panel_type: "text".to_owned(),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            is_archived: false,
            properties: Map::new(),
        };
        assert!(panel_text(&panel).is_err());
        panel
            .properties
            .insert("text".to_owned(), serde_json::json!("Hi"));
        assert_eq!(panel_text(&panel).unwrap().as_deref(), Some("Hi"));
        panel.panel_type = "tabs".to_owned();
        assert_eq!(panel_text(&panel).unwrap(), None);
        panel.panel_type = "query".to_owned();
        assert!(panel_text(&panel).is_err());
    }
}
