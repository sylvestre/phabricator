//! The member writes of the project pages, made as PHP's make them: adding
//! members (`/project/members/<id>/add/`,
//! `PhabricatorProjectMembersAddController`), removing a member or a
//! watcher (`/project/{members|watchers}/<id>/remove/`,
//! `PhabricatorProjectMembersRemoveController`) and silencing the mail to
//! members (`/project/silence/<id>/`, `PhabricatorProjectSilenceController`),
//! through the Rust project editor with a web content source; and the
//! dialog before the first subproject (`/project/warning/<id>/`,
//! `PhabricatorProjectSubprojectWarningController`), which only redirects
//! to PHP's form.
//!
//! The forms post PHP's fields (`memberPHIDs[]`, `phid`), so the replay of
//! a post Rust leaves to PHP is PHP's own. Without a JavaScript tokenizer,
//! the "Add Members" dialog takes usernames in its own `rust.add` input: a
//! post with names doesn't write, it shows the dialog again with their
//! users checked, so that the post which writes is a PHP form post.
//!
//! Left to PHP (`421`, before writing): projects the viewer can't edit
//! (PHP raises a policy exception), workflow (Ajax) requests, invalid
//! CSRF tokens, silencing subprojects, milestones and parents, and every
//! edit the editor leaves to PHP, which includes subprojects, archived
//! projects, PHIDs which are not users and edits without effect.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing};
use phab_app_project::web::{self, Change, Operation};
use phab_app_project::{Project, storage};
use phab_core::Phid;
use phab_policy::Capability;

use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::project_edit::{apply, dialog_page, has, load};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{actions, web_edit};

/// `PhabricatorProjectSilencedEdgeType`.
const SILENCED: i64 = 61;

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// The project `id` the viewer may edit: PHP's query requires `CAN_EDIT`
/// and raises a policy exception, which PHP explains.
async fn editable<S: WebState>(state: &S, viewer: &WebUser, id: i64) -> Result<Project, PageError> {
    let project = load(state, viewer, id).await?;
    if !has(state, viewer, &project, Capability::EDIT).await? {
        return Err(PageError::Fallback("projects the viewer can't edit"));
    }
    Ok(project)
}

/// `supportsEditMembers()`: neither a milestone nor a parent.
async fn supports_edit_members<S: WebState>(
    state: &S,
    project: &Project,
) -> Result<bool, PageError> {
    let (_, has_subprojects) = web::membership_flags(state.database(), project).await?;
    Ok(!project.is_milestone() && !has_subprojects)
}

/// A user the dialog lists, checked.
pub(crate) struct Checked {
    phid: String,
    name: String,
}

#[derive(Template)]
#[template(path = "project_members_add.html")]
pub(crate) struct AddPage {
    shell: Shell,
    action: String,
    cancel: String,
    users: Vec<Checked>,
    error: Option<String>,
    adding: String,
}

/// `memberPHIDs`, as `getArr()` reads it: its values, in order.
fn member_phids(request: &LoginRequest) -> Vec<String> {
    request
        .array("memberPHIDs")
        .into_iter()
        .map(|(_, value)| value)
        .collect()
}

/// The dialog, with `phids` checked and the names which didn't resolve.
async fn add_page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    uri: &Uri,
    phids: &[String],
    (error, adding): (Option<String>, String),
) -> Result<Response, PageError> {
    let ids: Vec<Phid> = phids.iter().map(|phid| Phid::new(phid.as_str())).collect();
    let users = phab_auth::user::load_users_by_phid(state.database(), &ids).await?;
    let users = phids
        .iter()
        .map(|phid| Checked {
            name: users
                .iter()
                .find(|user| user.phid.as_str() == phid)
                .map_or_else(|| phid.clone(), |user| user.user_name.clone()),
            phid: phid.clone(),
        })
        .collect();
    let id = uri.path().to_owned();
    Ok(Page(AddPage {
        shell: Shell::for_user(state.config(), viewer, "Add Members", current(uri)),
        cancel: id.trim_end_matches("add/").to_owned(),
        action: id,
        users,
        error,
        adding,
    })
    .into_response())
}

/// `GET|POST /project/members/{id}/add/`.
pub(crate) async fn add<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let project = editable(state, &viewer, id).await?;
    let done = format!("/project/members/{id}/");
    if !supports_edit_members(state, &project).await? {
        let content = (
            "Unsupported Project",
            vec![
                "Parent projects and milestones do not support adding members. You can add \
                 members directly to any non-parent subproject."
                    .to_owned(),
            ],
            None,
        );
        return Ok(dialog_page(state, &viewer, &uri, content, Vec::new(), done).into_response());
    }
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if !request.is_post() {
        return add_page(state, &viewer, &uri, &[], (None, String::new())).await;
    }
    web_edit::check_form_post(state, &viewer, &request)?;
    let mut phids = member_phids(&request);
    let typed = request.str("rust.add").unwrap_or_default().to_owned();
    let names = actions::names(&typed);
    if !names.is_empty() {
        // Rust's own input: show the dialog again with the users checked.
        if let Some(name) = names.iter().find(|name| name.is_project) {
            let error = format!("Members are users: #{} is a project.", name.name);
            return add_page(state, &viewer, &uri, &phids, (Some(error), typed)).await;
        }
        match actions::resolve(state, &viewer, &names).await {
            Ok(resolved) => {
                for (phid, _) in resolved {
                    if !phids.iter().any(|known| known == phid.as_str()) {
                        phids.push(phid.into_string());
                    }
                }
                return add_page(state, &viewer, &uri, &phids, (None, String::new())).await;
            }
            Err(PageError::Invalid(error)) => {
                return add_page(state, &viewer, &uri, &phids, (Some(error), typed)).await;
            }
            Err(other) => return Err(other),
        }
    }
    let changes = vec![Change::Members(Operation::Add, phids)];
    apply(state, &viewer, project, changes).await?;
    Ok(request.redirect(state.config(), &done))
}

/// `GET|POST /project/{members|watchers}/{id}/remove/`.
async fn remove<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (id, is_watcher): (i64, bool),
    method: Method,
    (uri, headers, body): (Uri, HeaderMap, Bytes),
) -> Result<Response, PageError> {
    let project = editable(state, viewer, id).await?;
    if !is_watcher && !supports_edit_members(state, &project).await? {
        return Err(PageError::NotFound);
    }
    let done = format!("/project/members/{id}/");
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let phid = request.str("phid").unwrap_or_default().to_owned();
    if request.is_post() {
        web_edit::check_form_post(state, viewer, &request)?;
        let removed = vec![phid];
        let changes = vec![if is_watcher {
            Change::Watchers(Operation::Remove, removed)
        } else {
            Change::Members(Operation::Remove, removed)
        }];
        apply(state, viewer, project, changes).await?;
        return Ok(request.redirect(state.config(), &done));
    }
    let users =
        phab_auth::user::load_users_by_phid(state.database(), &[Phid::new(phid.as_str())]).await?;
    let Some(user) = users.first() else {
        // PHP names other objects by their handle.
        return Err(PageError::Fallback("removing objects which are not users"));
    };
    let content = if is_watcher {
        (
            "Remove Watcher",
            vec![format!(
                "Remove {} as a watcher of {}?",
                user.user_name, project.name
            )],
            Some("Remove Watcher"),
        )
    } else {
        (
            "Remove Member",
            vec![format!(
                "Remove {} as a project member of {}?",
                user.user_name, project.name
            )],
            Some("Remove Member"),
        )
    };
    let hidden = vec![("phid", phid)];
    Ok(dialog_page(state, viewer, &uri, content, hidden, done).into_response())
}

/// `/project/members/{id}/remove/`.
pub(crate) async fn remove_member<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    remove(
        state.as_ref(),
        &viewer,
        (id, false),
        method,
        (uri, headers, body),
    )
    .await
}

/// `/project/watchers/{id}/remove/`.
pub(crate) async fn remove_watcher<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    remove(
        state.as_ref(),
        &viewer,
        (id, true),
        method,
        (uri, headers, body),
    )
    .await
}

/// `GET|POST /project/silence/{id}/`: the viewer's mail as a member.
pub(crate) async fn silence<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let project = load(state, &viewer, id).await?;
    if project.parent_phid.is_some() || !supports_edit_members(state, &project).await? {
        // Their members are their ancestors' or their subprojects'.
        return Err(PageError::Fallback(
            "silencing subprojects, milestones and parents",
        ));
    }
    let done = format!("/project/members/{id}/");
    let database = state.database();
    let members = storage::load_edge_destinations(
        database,
        &project.phid,
        phab_db::edges::PROJECT_HAS_MEMBER,
    )
    .await?;
    if !members.contains(&viewer.user.phid) {
        let content = (
            "Not a Member",
            vec![
                "You are not a project member, so you do not receive mail sent to members of \
                 this project."
                    .to_owned(),
            ],
            None,
        );
        return Ok(dialog_page(state, &viewer, &uri, content, Vec::new(), done).into_response());
    }
    let is_silenced = storage::load_edge_destinations(database, &project.phid, SILENCED)
        .await?
        .contains(&viewer.user.phid);
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if request.is_post() {
        web_edit::check_form_post(state, &viewer, &request)?;
        // `isDialogFormPost()`: other posts show the dialog again.
        if matches!(request.str("__dialog__"), None | Some("" | "0")) {
            return Err(PageError::Fallback("posts which aren't dialog posts"));
        }
        let operation = if is_silenced {
            Operation::Remove
        } else {
            Operation::Add
        };
        let actor = viewer.user.phid.as_str().to_owned();
        apply(
            state,
            &viewer,
            project,
            vec![Change::Silenced(operation, vec![actor])],
        )
        .await?;
        return Ok(request.redirect(state.config(), &done));
    }
    let content = if is_silenced {
        (
            "Enable Mail",
            "When mail is sent to members of this project, you will receive a copy.",
            "Enable Project Mail",
        )
    } else {
        (
            "Disable Mail",
            "When mail is sent to members of this project, you will no longer receive a copy.",
            "Disable Project Mail",
        )
    };
    let (title, paragraph, submit) = content;
    let hidden = vec![("__dialog__", "1".to_owned())];
    Ok(dialog_page(
        state,
        &viewer,
        &uri,
        (title, vec![paragraph.to_owned()], Some(submit)),
        hidden,
        done,
    )
    .into_response())
}

/// `GET|POST /project/warning/{id}/`: the post goes on to PHP's form of a
/// new subproject.
pub(crate) async fn warning<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let project = load(state, &viewer, id).await?;
    if !has(state, &viewer, &project, Capability::EDIT).await? {
        return Err(PageError::NotFound);
    }
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    if request.is_post() {
        web_edit::check_form_post(state, &viewer, &request)?;
        return Ok(request.redirect(state.config(), &format!("/project/edit/?parent={id}")));
    }
    let content = (
        "Convert to Parent Project",
        vec![
            "Creating a project's first subproject moves all members to become members of the \
             subproject instead."
                .to_owned(),
            "See Projects User Guide in the documentation for details. This process can not \
             be undone."
                .to_owned(),
        ],
        Some("Convert Project"),
    );
    let cancel = format!("/project/subprojects/{id}/");
    Ok(
        dialog_page(state, &viewer, &uri, content, Vec::new(), cancel)
            .with_link(
                "Projects User Guide",
                crate::diffusion::manage::doclink("Projects User Guide"),
            )
            .into_response(),
    )
}

/// PHP's routes of the member writes.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/project/members/{id}/add/",
            routing::get(add::<S>).post(add::<S>),
        )
        .route(
            "/project/members/{id}/remove/",
            routing::get(remove_member::<S>).post(remove_member::<S>),
        )
        .route(
            "/project/watchers/{id}/remove/",
            routing::get(remove_watcher::<S>).post(remove_watcher::<S>),
        )
        .route(
            "/project/silence/{id}/",
            routing::get(silence::<S>).post(silence::<S>),
        )
        .route(
            "/project/warning/{id}/",
            routing::get(warning::<S>).post(warning::<S>),
        )
}
