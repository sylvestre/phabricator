//! The project writes of the web interface, made as PHP's make them: the
//! edit form (`/project/edit/<id>/`, `PhabricatorProjectEditEngine`), joining
//! and leaving (`/project/update/<id>/{join|leave}/`,
//! `PhabricatorProjectUpdateController`), watching and unwatching
//! (`/project/{watch|unwatch}/<id>/`, `PhabricatorProjectWatchController`).
//! They run the Rust project editor ([`phab_app_project::web`]) with a web
//! content source; their GET pages are the forms and dialogs PHP shows.
//!
//! The edit form posts PHP's fields (`name`, the description's
//! `std:project:internal:description`, `icon`, `color`, `slugs`,
//! `policy.view`, `policy.edit`, `policy.join`), so the replay of a post
//! Rust leaves to PHP is PHP's own form post. Rust writes the name, icon,
//! color and hashtags; PHP's other transactions have no effect when their
//! values didn't change, and are dropped as `setContinueOnNoEffect()`
//! does. Left to PHP (`421`, before writing): changes to the description
//! or the policies, values the form doesn't offer, workflow (Ajax)
//! requests, invalid CSRF tokens, milestones, subprojects and parents,
//! archived projects, projects the viewer can't edit, installs with custom
//! fields, subtypes, spaces or configured project forms, and every edit
//! the editor leaves to PHP, which includes the validation errors PHP shows
//! on the form and edits without effect.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing};
use phab_app_project::web::{self, Change, Choices, Operation};
use phab_app_project::{Project, policy, storage};
use phab_conduit::MethodError;
use phab_core::Phid;
use phab_policy::{Capability, application};
use serde_json::Map;

use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// The description's custom field key, which is its input's name.
const DESCRIPTION: &str = "std:project:internal:description";
/// `PhabricatorCookies::COOKIE_SUBMIT`.
const SUBMIT_COOKIE: &str = "phabricator.submit";

/// A confirmation dialog, as PHP renders `newDialog()` without a
/// workflow: a page with the form.
#[derive(Template)]
#[template(path = "project_dialog.html")]
pub(crate) struct DialogPage {
    shell: Shell,
    title: &'static str,
    paragraphs: Vec<&'static str>,
    action: String,
    /// Hidden inputs besides the CSRF token and `__form__`.
    hidden: Vec<(&'static str, String)>,
    submit: Option<&'static str>,
    cancel: String,
}

/// A policy option of the form.
pub(crate) struct PolicyOption {
    value: String,
    label: String,
    selected: bool,
}

/// A policy field of the form.
pub(crate) struct PolicyField {
    key: &'static str,
    label: &'static str,
    options: Vec<PolicyOption>,
}

/// An icon or color option.
pub(crate) struct Choice {
    value: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "project_edit.html")]
pub(crate) struct EditPage {
    shell: Shell,
    id: i64,
    name: String,
    project_name: String,
    description: String,
    icons: Vec<Choice>,
    colors: Vec<Choice>,
    slugs: String,
    policies: Vec<PolicyField>,
}

fn method_error(error: MethodError) -> PageError {
    match error {
        MethodError::Unsupported(reason) => PageError::Fallback(reason),
        // PHP explains its refusals on the form or in a dialog.
        MethodError::Conduit { .. } => PageError::Fallback("edits PHP refuses"),
        MethodError::Internal(error) => PageError::Internal(Box::new(std::io::Error::other(error))),
    }
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// The project `id` the viewer sees: PHP answers 404 for the others.
async fn load<S: WebState>(state: &S, viewer: &WebUser, id: i64) -> Result<Project, PageError> {
    viewer
        .require_application(state, application::PROJECT)
        .await?;
    let project = storage::load_by_id(state.database(), id)
        .await?
        .ok_or(PageError::NotFound)?;
    policy::visible_projects(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        vec![project],
        state.now(),
    )
    .await
    .map_err(internal)?
    .0
    .pop()
    .ok_or(PageError::NotFound)
}

async fn has<S: WebState>(
    state: &S,
    viewer: &WebUser,
    project: &Project,
    capability: Capability,
) -> Result<bool, PageError> {
    policy::has_capability(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        project,
        capability,
        state.now(),
    )
    .await
    .map_err(internal)
}

/// `CAN_JOIN`, which project editors always have.
async fn can_join<S: WebState>(
    state: &S,
    viewer: &WebUser,
    project: &Project,
) -> Result<bool, PageError> {
    Ok(has(state, viewer, project, Capability::EDIT).await?
        || has(state, viewer, project, Capability::JOIN).await?)
}

/// Runs the editor with `changes` as the viewer.
async fn apply<S: WebState>(
    state: &S,
    viewer: &WebUser,
    project: Project,
    changes: Vec<Change>,
) -> Result<(), PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let context = web_edit::context(state);
    let request = web_edit::request(state, &context, viewer, Map::new());
    web::apply(&request, project, changes)
        .await
        .map_err(method_error)?;
    Ok(())
}

fn dialog<S: WebState>(
    state: &S,
    viewer: &WebUser,
    uri: &Uri,
    dialog: (&'static str, Vec<&'static str>, Option<&'static str>),
    hidden: Vec<(&'static str, String)>,
    cancel: String,
) -> Response {
    let (title, paragraphs, submit) = dialog;
    Page(DialogPage {
        shell: Shell::for_user(state.config(), viewer, title, current(uri)),
        title,
        paragraphs,
        action: uri.path().to_owned(),
        hidden,
        submit,
        cancel,
    })
    .into_response()
}

/// `GET|POST /project/update/{id}/{action}/`: joining or leaving.
pub(crate) async fn update<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((id, action)): Path<(i64, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let is_join = match action.as_str() {
        "join" => true,
        "leave" => false,
        _ => return Err(PageError::NotFound),
    };
    let state = state.as_ref();
    let project = load(state, &viewer, id).await?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if is_join && !can_join(state, &viewer, &project).await? {
        // The query requires `CAN_JOIN`.
        return Err(if request.is_post() {
            PageError::Fallback("joins the viewer may not make")
        } else {
            PageError::NotFound
        });
    }
    let (locked, has_subprojects) = web::membership_flags(state.database(), &project).await?;
    if project.is_milestone() || has_subprojects {
        // PHP explains in a dialog that members can't change.
        return Err(PageError::Fallback("members of parents and milestones"));
    }
    let done = format!("/project/members/{id}/");
    if request.is_post() {
        web_edit::check_form_post(state, &viewer, &request)?;
        let operation = if is_join {
            Operation::Add
        } else {
            Operation::Remove
        };
        let actor = viewer.user.phid.as_str().to_owned();
        let changes = vec![Change::Members(operation, vec![actor])];
        apply(state, &viewer, project, changes).await?;
        return Ok(request.redirect(state.config(), &done));
    }
    let content = if is_join {
        (
            "Join Project",
            vec![
                "Join this project? You will become a member and enjoy whatever benefits \
                 membership may confer.",
            ],
            Some("Join Project"),
        )
    } else if !locked || has(state, &viewer, &project, Capability::EDIT).await? {
        (
            "Leave Project",
            vec![
                "Your tremendous contributions to this project will be sorely missed. Are you \
                 sure you want to leave?",
            ],
            Some("Leave Project"),
        )
    } else {
        (
            "Membership Locked",
            vec!["Membership for this project is locked. You can not leave."],
            None,
        )
    };
    Ok(dialog(state, &viewer, &uri, content, Vec::new(), done))
}

/// `GET|POST /project/{watch|unwatch}/{id}/`.
async fn watch<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (id, is_watch): (i64, bool),
    method: Method,
    (uri, headers, body): (Uri, HeaderMap, Bytes),
) -> Result<Response, PageError> {
    let project = load(state, viewer, id).await?;
    if project.parent_phid.is_some() {
        // Watching an ancestor watches its subprojects, which PHP says.
        return Err(PageError::Fallback("watching subprojects and milestones"));
    }
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let via = request.str("via").unwrap_or_default().to_owned();
    let done = if via == "profile" {
        format!("/project/profile/{id}/")
    } else {
        format!("/project/members/{id}/")
    };
    if request.is_post() {
        web_edit::check_form_post(state, viewer, &request)?;
        // `isDialogFormPost()`: other posts show the dialog again.
        if matches!(request.str("__dialog__"), None | Some("" | "0")) {
            return Err(PageError::Fallback("posts which aren't dialog posts"));
        }
        let operation = if is_watch {
            Operation::Add
        } else {
            Operation::Remove
        };
        let actor = viewer.user.phid.as_str().to_owned();
        let changes = vec![Change::Watchers(operation, vec![actor])];
        apply(state, viewer, project, changes).await?;
        return Ok(request.redirect(state.config(), &done));
    }
    let content = if is_watch {
        (
            "Watch Project?",
            vec![
                "Watching a project will let you monitor it closely. You will receive email and \
                 notifications about changes to every object tagged with projects you watch.",
                "Watching a project also watches all subprojects and milestones of that project.",
            ],
            Some("Watch Project"),
        )
    } else {
        (
            "Unwatch Project?",
            vec![
                "You will no longer receive email or notifications about every object associated \
                 with this project.",
            ],
            Some("Unwatch Project"),
        )
    };
    let hidden = vec![("__dialog__", "1".to_owned()), ("via", via)];
    Ok(dialog(state, viewer, &uri, content, hidden, done))
}

/// `/project/watch/{id}/`.
pub(crate) async fn watch_project<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    watch(
        state.as_ref(),
        &viewer,
        (id, true),
        method,
        (uri, headers, body),
    )
    .await
}

/// `/project/unwatch/{id}/`.
pub(crate) async fn unwatch_project<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    watch(
        state.as_ref(),
        &viewer,
        (id, false),
        method,
        (uri, headers, body),
    )
    .await
}

/// The global policies the policy controls offer
/// (`PhabricatorPolicyQuery::getGlobalPolicies()` and the project's
/// object policy), and their names.
fn global_policies(allow_public: bool) -> Vec<(&'static str, &'static str)> {
    let mut policies = Vec::new();
    if allow_public {
        policies.push(("public", "Public (No Login Required)"));
    }
    policies.extend([
        ("users", "All Users"),
        ("obj.project.members", "Project Members"),
        ("admin", "Administrators"),
        ("no-one", "No One"),
    ]);
    policies
}

/// The policy fields, with the project's values selected. Policies the
/// form doesn't list are named by their handle.
async fn policy_fields<S: WebState>(
    state: &S,
    viewer: &WebUser,
    project: &Project,
) -> Result<Vec<PolicyField>, PageError> {
    let allow_public = state
        .config()
        .value("policy.allow-public")
        .is_some_and(phab_core::php::is_truthy);
    let globals = global_policies(allow_public);
    let values = [
        ("policy.view", "Visible To", &project.view_policy),
        ("policy.edit", "Editable By", &project.edit_policy),
        ("policy.join", "Joinable By", &project.join_policy),
    ];
    let others: Vec<Phid> = values
        .iter()
        .filter(|(_, _, value)| !globals.iter().any(|(key, _)| key == value))
        .map(|(_, _, value)| Phid::new(value.as_str()))
        .filter(|phid| matches!(phid.phid_type().as_str(), "USER" | "PROJ"))
        .collect();
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &others).await?;
    Ok(values
        .into_iter()
        .map(|(key, label, value)| {
            let mut options: Vec<PolicyOption> = globals
                .iter()
                .map(|(policy, name)| PolicyOption {
                    value: (*policy).to_owned(),
                    label: (*name).to_owned(),
                    selected: policy == value,
                })
                .collect();
            if !options.iter().any(|option| option.selected) {
                let phid = Phid::new(value.as_str());
                let label = match phid.phid_type().as_str() {
                    "PROJ" => format!(
                        "Members of {}",
                        harbormaster::handle(&handles, &phid).full_name
                    ),
                    "USER" => harbormaster::handle(&handles, &phid).name.clone(),
                    "PLCY" => "Custom Policy".to_owned(),
                    _ => value.clone(),
                };
                options.insert(
                    0,
                    PolicyOption {
                        value: value.clone(),
                        label,
                        selected: true,
                    },
                );
            }
            PolicyField {
                key,
                label,
                options,
            }
        })
        .collect())
}

/// The project `id` the edit form edits, or why PHP shows the form.
async fn editable<S: WebState>(state: &S, viewer: &WebUser, id: i64) -> Result<Project, PageError> {
    let project = load(state, viewer, id).await?;
    if project.is_milestone() {
        return Err(PageError::Fallback(
            "milestones, whose form has fewer fields",
        ));
    }
    if !has(state, viewer, &project, Capability::EDIT).await? {
        // PHP explains the policy.
        return Err(PageError::Fallback("projects the viewer can't edit"));
    }
    let context = web_edit::context(state);
    let request = web_edit::request(state, &context, viewer, Map::new());
    if let Some(reason) = web::unported_form(&request).await.map_err(method_error)? {
        return Err(PageError::Fallback(reason));
    }
    Ok(project)
}

/// `getStrList()`: the items between whitespace and commas.
fn str_list(text: &str) -> Vec<String> {
    text.split(|character: char| {
        matches!(character, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ',')
    })
    .filter(|item| !item.is_empty())
    .map(str::to_owned)
    .collect()
}

/// What the form posts, once each field is read as PHP reads it.
struct Submitted {
    name: String,
    description: Option<String>,
    icon: String,
    color: String,
    slugs: Vec<String>,
    policies: [String; 3],
}

impl Submitted {
    fn from_request(request: &LoginRequest) -> Result<Self, &'static str> {
        let field = |key: &str| {
            request
                .str(key)
                .map(str::to_owned)
                .ok_or("form posts without every field")
        };
        if request.exists("slugs[]") || !request.array("slugs").is_empty() {
            return Err("hashtags as a list");
        }
        let description = field(DESCRIPTION)?;
        Ok(Self {
            name: field("name")?,
            // `readValueFromRequest()`: an empty value is none.
            description: Some(description).filter(|text| !text.is_empty()),
            icon: field("icon")?,
            color: field("color")?,
            slugs: str_list(&field("slugs")?),
            policies: [
                field("policy.view")?,
                field("policy.edit")?,
                field("policy.join")?,
            ],
        })
    }
}

/// The editor's changes for the fields which differ from the project's
/// values, in the form's order; the others have no effect. Changes Rust
/// doesn't write are left to PHP.
async fn changes<S: WebState>(
    state: &S,
    project: &Project,
    submitted: Submitted,
) -> Result<Vec<Change>, PageError> {
    let database = state.database();
    let mut changes = Vec::new();
    if submitted.name != project.name {
        changes.push(Change::Name(submitted.name));
    }
    let stored = storage::load_stored_description(database, &project.phid).await?;
    if submitted.description != stored {
        return Err(PageError::Fallback("description changes"));
    }
    let choices = Choices::load(state.config());
    if submitted.icon != project.icon {
        if !choices.has_icon(&submitted.icon) {
            return Err(PageError::Fallback("icons the form doesn't offer"));
        }
        changes.push(Change::Icon(submitted.icon));
    }
    if submitted.color != project.color {
        if !choices.has_color(&submitted.color) {
            return Err(PageError::Fallback("colors the form doesn't offer"));
        }
        changes.push(Change::Color(submitted.color));
    }
    let slugs = web::additional_slugs(database, project).await?;
    if submitted.slugs != slugs {
        changes.push(Change::Slugs(submitted.slugs));
    } else if !web::slugs_are_normal(&slugs) {
        // An unchanged value is a default transaction (`core.default`),
        // which hashtags still in an old form give an effect.
        return Err(PageError::Fallback("hashtags which aren't normalized"));
    }
    let current = [
        &project.view_policy,
        &project.edit_policy,
        &project.join_policy,
    ];
    if submitted
        .policies
        .iter()
        .zip(current)
        .any(|(submitted, current)| submitted != current)
    {
        return Err(PageError::Fallback("policy changes"));
    }
    Ok(changes)
}

async fn edit_form<S: WebState>(
    state: &S,
    viewer: &WebUser,
    project: &Project,
    uri: &Uri,
) -> Result<EditPage, PageError> {
    let database = state.database();
    let choices = Choices::load(state.config());
    let mut icons: Vec<Choice> = choices
        .icons
        .iter()
        .map(|icon| Choice {
            value: icon.key.clone(),
            label: icon.name.clone(),
            selected: icon.key == project.icon,
        })
        .collect();
    if !icons.iter().any(|icon| icon.selected) {
        icons.insert(
            0,
            Choice {
                value: project.icon.clone(),
                label: project.icon.clone(),
                selected: true,
            },
        );
    }
    let mut colors: Vec<Choice> = choices
        .colors
        .iter()
        .map(|(key, name)| Choice {
            value: key.clone(),
            label: name.clone(),
            selected: *key == project.color,
        })
        .collect();
    if !colors.iter().any(|color| color.selected) {
        colors.insert(
            0,
            Choice {
                value: project.color.clone(),
                label: project.color.clone(),
                selected: true,
            },
        );
    }
    let slugs = web::additional_slugs(database, project).await?;
    let description = storage::load_stored_description(database, &project.phid)
        .await?
        .unwrap_or_default();
    let title = format!("Edit Project: {}", project.name);
    Ok(EditPage {
        shell: Shell::for_user(state.config(), viewer, &title, current(uri)),
        id: project.id,
        name: project.name.clone(),
        project_name: title,
        description,
        icons,
        colors,
        slugs: slugs.join(", "),
        policies: policy_fields(state, viewer, project).await?,
    })
}

/// `GET|POST /project/edit/{id}/`.
pub(crate) async fn edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    // `?parent=`, `?milestone=` and the like are for new projects.
    if uri.query().is_some_and(|query| !query.is_empty()) {
        return Err(PageError::Fallback("edit forms with parameters"));
    }
    let project = editable(state, &viewer, id).await?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if !request.is_post() {
        return Ok(Page(edit_form(state, &viewer, &project, &uri).await?).into_response());
    }
    web_edit::check_form_post(state, &viewer, &request)?;
    let submitted = Submitted::from_request(&request).map_err(PageError::Fallback)?;
    let changes = changes(state, &project, submitted).await?;
    if changes.is_empty() {
        return Err(PageError::Fallback("edits without effect"));
    }
    apply(state, &viewer, project, changes).await?;
    // `newEditResponse()`.
    let mut request = request;
    request.set_temporary_cookie(
        state.config(),
        SUBMIT_COOKIE,
        "edit-engine/projects.project",
        state.now(),
    );
    Ok(request.redirect(state.config(), &format!("/project/manage/{id}/")))
}

/// PHP's routes of the project writes.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/project/update/{id}/{action}/",
            routing::get(update::<S>).post(update::<S>),
        )
        .route(
            "/project/watch/{id}/",
            routing::get(watch_project::<S>).post(watch_project::<S>),
        )
        .route(
            "/project/unwatch/{id}/",
            routing::get(unwatch_project::<S>).post(unwatch_project::<S>),
        )
        .route(
            "/project/edit/{id}/",
            routing::get(edit::<S>).post(edit::<S>),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_split_like_php() {
        assert_eq!(str_list(" a, b,,c\td "), ["a", "b", "c", "d"]);
        assert_eq!(str_list(" , "), Vec::<String>::new());
    }

    #[test]
    fn public_is_offered_when_allowed() {
        assert_eq!(global_policies(false)[0].0, "users");
        assert_eq!(global_policies(true)[0].0, "public");
    }
}
