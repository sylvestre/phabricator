//! Project pages: `/tag/<slug>/`, `/project/view/<id>/` and
//! `/project/profile/<id>/` (`PhabricatorProjectViewController` and
//! `PhabricatorProjectProfileController`), `/project/members/<id>/` and
//! `/project/subprojects/<id>/`, and the builtin queries of the project
//! list (`/project/`, `/project/query/<key>/`, `PhabricatorProjectListController`).
//! Workboards went with Maniphest, so every project shows its profile.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_app_differential::storage as differential;
use phab_app_project::{Project, policy, storage, web};
use phab_core::{Phid, PhidType};
use phab_db::edges;
use phab_policy::{Viewer, application};
use phab_remarkup::{Markup, Mode};

use crate::error::PageError;
use crate::remarkup;
use crate::response::Page;
use crate::search_form::{self, Control, Field, SavedQuery};
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// How many members, watchers and revisions the page lists.
const LIST_LIMIT: usize = 25;
/// `PhabricatorProjectSilencedEdgeType`.
const SILENCED: i64 = 61;

/// A link in one of the lists.
pub(crate) struct Item {
    /// The user's or project's PHID.
    phid: String,
    name: String,
    uri: String,
    closed: bool,
}

#[derive(Template)]
#[template(path = "project.html")]
pub(crate) struct ProjectPage {
    id: i64,
    shell: Shell,
    name: String,
    icon: String,
    color: String,
    archived: bool,
    milestone: bool,
    parent: Option<Item>,
    hashtag: Option<String>,
    description: Option<Markup>,
    members: Vec<Item>,
    member_count: usize,
    watcher_count: usize,
    children: Vec<Item>,
    revisions: Vec<Item>,
    revision_count: usize,
}

impl ProjectPage {
    fn description_html(&self) -> &str {
        self.description.as_ref().map_or("", |description| {
            std::str::from_utf8(description.as_bytes()).unwrap_or("")
        })
    }
}

/// A project the viewer may see, with its ancestors, or why not.
async fn visible<S: WebState>(
    state: &S,
    viewer: &Viewer,
    project: Project,
) -> Result<(Project, policy::Graph), PageError> {
    let (mut visible, graph) = policy::visible_projects(
        state.database(),
        state.config(),
        viewer,
        vec![project],
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    // The project exists: PHP's executeOne() raises a policy exception.
    let project = visible.pop().ok_or(PageError::PolicyDenied)?;
    Ok((project, graph))
}

async fn user_items<S: WebState>(state: &S, phids: &[Phid]) -> Result<Vec<Item>, PageError> {
    let mut users = phab_auth::user::load_users_by_phid(state.database(), phids).await?;
    users.sort_by_cached_key(|user| user.user_name.to_lowercase());
    Ok(users
        .into_iter()
        .map(|user| Item {
            phid: user.phid.as_str().to_owned(),
            uri: format!("/p/{}/", user.user_name),
            closed: user.is_disabled,
            name: user.user_name,
        })
        .collect())
}

/// The revisions tagged with the project which the viewer may see, newest
/// first.
async fn tagged_revisions<S: WebState>(
    state: &S,
    viewer: &Viewer,
    project: &Phid,
) -> Result<Vec<Item>, PageError> {
    let phids: Vec<Phid> =
        storage::load_edge_destinations(state.database(), project, storage::PROJECT_HAS_OBJECT)
            .await?
            .into_iter()
            .filter(|phid| phid.phid_type() == PhidType::REVISION)
            .collect();
    let revisions = differential::load_revisions(state.database(), &phids).await?;
    let mut revisions = differential::visible_revisions(
        state.database(),
        state.config(),
        viewer,
        revisions,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    revisions.sort_by_key(|revision| std::cmp::Reverse(revision.id));
    Ok(revisions
        .into_iter()
        .map(|revision| Item {
            phid: revision.phid.as_str().to_owned(),
            name: format!("D{}: {}", revision.id, revision.title),
            uri: format!("/D{}", revision.id),
            closed: revision.is_closed(),
        })
        .collect())
}

/// The page of a project the viewer may see.
async fn render<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (project, graph): (Project, policy::Graph),
    current_uri: &str,
) -> Result<Page<ProjectPage>, PageError> {
    let as_viewer = viewer.user.viewer();
    let database = state.database();

    let description = storage::load_description(database, &project.phid).await?;
    let description = if description.is_empty() {
        None
    } else {
        Some(remarkup::render(state, &as_viewer, &description, Mode::Html).await?)
    };

    let member_phids =
        storage::load_edge_destinations(database, &project.phid, edges::PROJECT_HAS_MEMBER).await?;
    let watcher_count =
        storage::load_edge_destinations(database, &project.phid, storage::OBJECT_HAS_WATCHER)
            .await?
            .len();
    let mut members = user_items(state, &member_phids).await?;
    let member_count = members.len();
    members.truncate(LIST_LIMIT);

    let children = storage::load_children(database, &project.phid).await?;
    let (children, children_graph) =
        policy::visible_projects(database, state.config(), &as_viewer, children, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    let children = children
        .iter()
        .map(|child| Item {
            phid: child.phid.as_str().to_owned(),
            name: child.display_name(children_graph.parent(child)),
            uri: child.uri(),
            closed: child.is_archived(),
        })
        .collect();

    let mut revisions = tagged_revisions(state, &as_viewer, &project.phid).await?;
    let revision_count = revisions.len();
    revisions.truncate(LIST_LIMIT);

    let parent = graph.parent(&project).map(|parent| Item {
        phid: parent.phid.as_str().to_owned(),
        name: parent.name.clone(),
        uri: parent.uri(),
        closed: parent.is_archived(),
    });
    let name = project.display_name(graph.parent(&project));
    Ok(Page(ProjectPage {
        id: project.id,
        shell: Shell::for_user(state.config(), viewer, &name, current_uri),
        name,
        icon: project.icon.clone(),
        color: project.color.clone(),
        archived: project.is_archived(),
        milestone: project.is_milestone(),
        parent,
        hashtag: project.primary_slug.clone(),
        description,
        members,
        member_count,
        watcher_count,
        children,
        revisions,
        revision_count,
    }))
}

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// `GET /tag/{slug}/`: other hashtags of the project redirect to its
/// primary one, like PHP, once the viewer is known to see the project.
pub(crate) async fn by_slug<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(slug): Path<String>,
    uri: Uri,
) -> Result<Response, PageError> {
    viewer
        .require_application(&*state, application::PROJECT)
        .await?;
    let lowered = slug.to_lowercase();
    let (_, project) = storage::load_by_slug(state.database(), std::slice::from_ref(&lowered))
        .await?
        .into_iter()
        .next()
        .ok_or(PageError::NotFound)?;
    let (project, graph) = visible(state.as_ref(), &viewer.user.viewer(), project).await?;
    if let Some(primary) = project
        .primary_slug
        .as_deref()
        .filter(|primary| *primary != slug)
    {
        let mut response = StatusCode::FOUND.into_response();
        if let Ok(location) = HeaderValue::from_str(&project_uri(primary)) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        return Ok(response);
    }
    Ok(
        render(state.as_ref(), &viewer, (project, graph), current(&uri))
            .await?
            .into_response(),
    )
}

fn project_uri(slug: &str) -> String {
    let encoded: String = form_urlencoded::byte_serialize(slug.as_bytes()).collect();
    format!("/tag/{encoded}/")
}

/// A visible project by ID, or why not.
async fn visible_by_id<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<(Project, policy::Graph), PageError> {
    viewer
        .require_application(state, application::PROJECT)
        .await?;
    let project = storage::load_by_id(state.database(), id)
        .await?
        .ok_or(PageError::NotFound)?;
    visible(state, &viewer.user.viewer(), project).await
}

#[derive(Template)]
#[template(path = "project_members.html")]
pub(crate) struct MembersPage {
    shell: Shell,
    id: i64,
    name: String,
    uri: String,
    members: Vec<Item>,
    watchers: Vec<Item>,
    is_member: bool,
    is_watcher: bool,
    is_silenced: bool,
    /// `CAN_EDIT` and `supportsEditMembers()`: members can be added and
    /// removed.
    can_edit_members: bool,
    can_edit: bool,
}

/// `GET /project/members/{id}/` (`PhabricatorProjectMembersViewController`):
/// every member and watcher, and the actions of the curtain.
pub(crate) async fn members<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<MembersPage>, PageError> {
    let (project, graph) = visible_by_id(state.as_ref(), &viewer, id).await?;
    let database = state.database();
    let member_phids =
        storage::load_edge_destinations(database, &project.phid, edges::PROJECT_HAS_MEMBER).await?;
    let watcher_phids =
        storage::load_edge_destinations(database, &project.phid, storage::OBJECT_HAS_WATCHER)
            .await?;
    let silenced = storage::load_edge_destinations(database, &project.phid, SILENCED).await?;
    let can_edit = policy::has_capability(
        database,
        state.config(),
        &viewer.user.viewer(),
        &project,
        phab_policy::Capability::EDIT,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    let (_, has_subprojects) = phab_app_project::web::membership_flags(database, &project).await?;
    let supports_edit = !project.is_milestone() && !has_subprojects;
    let name = project.display_name(graph.parent(&project));
    Ok(Page(MembersPage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("{} · Members and Watchers", project.name),
            current(&uri),
        ),
        id: project.id,
        name,
        uri: project.uri(),
        members: user_items(state.as_ref(), &member_phids).await?,
        watchers: user_items(state.as_ref(), &watcher_phids).await?,
        is_member: member_phids.contains(&viewer.user.phid),
        is_watcher: watcher_phids.contains(&viewer.user.phid),
        is_silenced: silenced.contains(&viewer.user.phid),
        can_edit_members: can_edit && supports_edit,
        can_edit,
    }))
}

#[derive(Template)]
#[template(path = "project_subprojects.html")]
pub(crate) struct SubprojectsPage {
    shell: Shell,
    name: String,
    uri: String,
    /// `None` for a milestone, which has neither.
    lists: Option<(Vec<Item>, Vec<Item>)>,
}

/// `GET /project/subprojects/{id}/` (`PhabricatorProjectSubprojectsController`):
/// the subprojects by name and the milestones by number.
pub(crate) async fn subprojects<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<SubprojectsPage>, PageError> {
    let (project, graph) = visible_by_id(state.as_ref(), &viewer, id).await?;
    let lists = if project.is_milestone() {
        None
    } else {
        let children = storage::load_children(state.database(), &project.phid).await?;
        let (children, children_graph) = policy::visible_projects(
            state.database(),
            state.config(),
            &viewer.user.viewer(),
            children,
            state.now(),
        )
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;
        let item = |child: &Project| Item {
            phid: child.phid.as_str().to_owned(),
            name: child.display_name(children_graph.parent(child)),
            uri: child.uri(),
            closed: child.is_archived(),
        };
        let mut subprojects: Vec<&Project> = children
            .iter()
            .filter(|child| !child.is_milestone())
            .collect();
        subprojects.sort_by_cached_key(|child| (child.name.to_lowercase(), child.id));
        let mut milestones: Vec<&Project> = children
            .iter()
            .filter(|child| child.is_milestone())
            .collect();
        milestones.sort_by_key(|child| (child.milestone_number, child.id));
        Some((
            subprojects.into_iter().map(item).collect(),
            milestones.into_iter().map(item).collect(),
        ))
    };
    let name = project.display_name(graph.parent(&project));
    Ok(Page(SubprojectsPage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("{} · Subprojects and Milestones", project.name),
            current(&uri),
        ),
        name,
        uri: project.uri(),
        lists,
    }))
}

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = search_form::PAGE_SIZE;
/// `PhabricatorProjectSearchEngine`.
const ENGINE: &str = "PhabricatorProjectSearchEngine";

/// A project of the list.
pub(crate) struct ListItem {
    name: String,
    uri: String,
    icon: String,
    color: String,
    hashtag: Option<String>,
    archived: bool,
    member: bool,
}

#[derive(Template)]
#[template(path = "project_list.html")]
pub(crate) struct ProjectList {
    shell: Shell,
    query: String,
    title: &'static str,
    /// The advanced search form of a saved query.
    edit_uri: Option<String>,
    items: Vec<ListItem>,
    next: Option<i64>,
}

/// The builtin queries (`getBuiltinQueryNames()`), for a logged-in viewer.
fn builtin_query(key: &str) -> Option<(&'static str, &'static str)> {
    Some(match key {
        "joined" => ("joined", "Joined"),
        "watching" => ("watching", "Watching"),
        "active" => ("active", "Active"),
        "all" => ("all", "All"),
        _ => return None,
    })
}

/// The parameters of a saved query which the Rust list runs only empty.
const EMPTY_ONLY: [&str; 6] = [
    "query",
    "subtypes",
    "ancestorPHIDs",
    "ids",
    "phids",
    "spacePHIDs",
];

/// A saved query, as `buildQueryFromParameters()` reads it.
fn saved_filters(saved: &SavedQuery) -> Result<web::SavedQuery, PageError> {
    let mut known = vec![
        "name",
        "slugs",
        "memberPHIDs",
        "watcherPHIDs",
        "status",
        "isMilestone",
        "isRoot",
        "minDepth",
        "maxDepth",
        "icons",
        "colors",
        "parentPHIDs",
        "order",
        "limit",
    ];
    known.extend(EMPTY_ONLY);
    saved.only(&known)?;
    for key in EMPTY_ONLY {
        let empty = match key {
            "query" => saved.text(key)?.is_empty(),
            _ => saved.list(key)?.is_empty(),
        };
        if !empty {
            return Err(PageError::Fallback("saved query parameters not ported"));
        }
    }
    let users = |key: &str| -> Result<Vec<String>, PageError> {
        let list = saved.list(key)?;
        if list
            .iter()
            .any(|token| search_form::is_function_token(token))
        {
            return Err(PageError::Fallback("functions in saved queries"));
        }
        Ok(list)
    };
    Ok(web::SavedQuery {
        name: saved.text("name")?,
        slugs: saved.list("slugs")?,
        members: users("memberPHIDs")?,
        watchers: users("watcherPHIDs")?,
        status: saved.text("status")?,
        is_milestone: saved.three_state("isMilestone")?,
        is_root: saved.three_state("isRoot")?,
        depth: (saved.int("minDepth")?, saved.int("maxDepth")?),
        icons: saved.list("icons")?,
        colors: saved.list("colors")?,
        parents: saved.list("parentPHIDs")?,
        order: saved.order()?,
    })
}

/// Projects, by their query, a page at a time.
async fn project_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &str,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<ProjectList>, PageError> {
    let after = crate::herald::cursor(query)?;
    let scope = crate::harbormaster::Scope::new(state, viewer);
    let request = scope.request();
    let (listed, title, edit_uri) = if let Some((key, title)) = builtin_query(key) {
        let viewer_phid = viewer.user.phid.as_str().to_owned();
        let list_query = web::ListQuery {
            member: (key == "joined").then(|| viewer_phid.clone()),
            watcher: (key == "watching").then(|| viewer_phid.clone()),
            active_only: key != "all",
        };
        let listed = web::list(&request, &list_query, after, PAGE_SIZE)
            .await
            .map_err(crate::harbormaster::method_error)?;
        (listed, title, None)
    } else {
        let saved = SavedQuery::load(state, viewer, ENGINE, key).await?;
        if let Some(reason) = web::unported_search(&request)
            .await
            .map_err(crate::harbormaster::method_error)?
        {
            return Err(PageError::Fallback(reason));
        }
        let filters = saved_filters(&saved)?;
        let listed = web::list_saved(&request, &filters, after, saved.page_size())
            .await
            .map_err(search_form::query_error)?;
        let edit_uri = format!("/project/query/advanced/?query={key}");
        (listed, "Advanced Search", Some(edit_uri))
    };
    let (projects, more) = listed;
    let database = state.database();
    let graph = policy::Graph::load(database, &projects).await?;
    let joined: Vec<Phid> = storage::load_member_projects(database, &viewer.user.phid)
        .await?
        .into_iter()
        .map(|project| project.phid)
        .collect();
    let next = if more {
        projects.last().map(|project| project.id)
    } else {
        None
    };
    let items = projects
        .iter()
        .map(|project| ListItem {
            name: project.display_name(graph.parent(project)),
            uri: project.uri(),
            icon: project.icon.clone(),
            color: project.color.clone(),
            hashtag: project.primary_slug.clone(),
            archived: project.is_archived(),
            member: joined.contains(&project.phid),
        })
        .collect();
    Ok(Page(ProjectList {
        shell: Shell::for_user(state.config(), viewer, "Projects", current(uri)),
        query: key.to_owned(),
        title,
        edit_uri,
        items,
        next,
    }))
}

/// The usernames of `phids` for the form's tokenizers, functions and
/// unknown PHIDs as they are.
async fn token_names<S: WebState>(state: &S, phids: &[String]) -> Result<String, PageError> {
    let ids: Vec<Phid> = phids.iter().map(|phid| Phid::new(phid.as_str())).collect();
    let users = phab_auth::user::load_users_by_phid(state.database(), &ids).await?;
    Ok(phids
        .iter()
        .map(|phid| {
            users
                .iter()
                .find(|user| user.phid.as_str() == phid)
                .map_or_else(|| phid.clone(), |user| user.user_name.clone())
        })
        .collect::<Vec<_>>()
        .join(", "))
}

/// The fields of the project search form, filled with `saved`.
async fn search_fields<S: WebState>(
    state: &S,
    saved: &SavedQuery,
) -> Result<Vec<Field>, PageError> {
    let choices = web::Choices::load(state.config());
    let status = Some(saved.text("status")?)
        .filter(|status| !status.is_empty())
        .unwrap_or_else(|| "active".to_owned());
    let order = saved.order()?.unwrap_or_else(|| "name".to_owned());
    let select = |options: &[(&str, &str)], selected: String| {
        Control::Select(
            options
                .iter()
                .map(|(value, label)| ((*value).to_owned(), (*label).to_owned()))
                .collect(),
            selected,
        )
    };
    let mut fields = vec![
        Field::text("query", "Query", saved.text("query")?),
        Field::text("name", "Name", saved.text("name")?),
        Field::text(
            "memberPHIDs",
            "Members",
            token_names(state, &saved.list("memberPHIDs")?).await?,
        ),
        Field::text(
            "watcherPHIDs",
            "Watchers",
            token_names(state, &saved.list("watcherPHIDs")?).await?,
        ),
        Field {
            key: "status",
            label: "Status",
            control: select(
                &[
                    ("active", "Show Only Active Projects"),
                    ("archived", "Show Only Archived Projects"),
                    ("all", "Show All Projects"),
                ],
                status,
            ),
        },
        Field::three_state(
            "isMilestone",
            "Milestones",
            ["(Show All)", "Show Only Milestones", "Hide Milestones"],
            saved.three_state("isMilestone")?,
        ),
        Field::three_state(
            "isRoot",
            "Root Projects",
            [
                "(Show All)",
                "Show Only Root Projects",
                "Hide Root Projects",
            ],
            saved.three_state("isRoot")?,
        ),
        Field {
            key: "icons",
            label: "Icons",
            control: Control::Checkboxes(
                choices
                    .icons
                    .iter()
                    .map(|icon| (icon.key.clone(), icon.name.clone()))
                    .collect(),
                saved.list("icons")?,
            ),
        },
        Field {
            key: "colors",
            label: "Colors",
            control: Control::Checkboxes(choices.colors.clone(), saved.list("colors")?),
        },
    ];
    // PHID fields show only once they have a value.
    for (key, label) in [
        ("parentPHIDs", "Parent Projects"),
        ("ancestorPHIDs", "Ancestor Projects"),
    ] {
        let list = saved.list(key)?;
        if !list.is_empty() {
            fields.push(Field::text(key, label, list.join(", ")));
        }
    }
    fields.push(Field {
        key: "order",
        label: "Order By",
        control: select(
            &[
                ("name", "Name"),
                ("newest", "Creation (Newest First)"),
                ("oldest", "Creation (Oldest First)"),
                ("relevance", "Relevance"),
            ],
            order,
        ),
    });
    Ok(fields)
}

/// `GET /project/query/advanced/[?query=<key>]`: the search form. Its
/// post saves the query, which PHP does.
async fn advanced<S: WebState>(
    state: &S,
    viewer: &WebUser,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Response, PageError> {
    let saved = match search_form::edited_key(query)? {
        Some(key) if builtin_query(&key).is_some() => {
            return Err(PageError::Fallback("editing builtin queries"));
        }
        Some(key) => SavedQuery::load(state, viewer, ENGINE, &key).await?,
        None => SavedQuery::empty(),
    };
    let fields = search_fields(state, &saved).await?;
    Ok(search_form::advanced_page(
        state,
        viewer,
        uri,
        ("Projects", "/project/"),
        &saved,
        fields,
    )
    .into_response())
}

/// `GET /project/`: the viewer's default query, the first builtin one
/// ("Joined") unless they saved or reordered queries.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ProjectList>, PageError> {
    viewer
        .require_application(&*state, application::PROJECT)
        .await?;
    if phab_app_harbormaster::web::has_named_queries(state.database(), &viewer.user.phid, ENGINE)
        .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    project_list(state.as_ref(), &viewer, "joined", query.as_deref(), &uri).await
}

/// `GET /project/query/{key}/`: a builtin or saved query, or the search
/// form.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Response, PageError> {
    viewer
        .require_application(&*state, application::PROJECT)
        .await?;
    if key == "advanced" {
        return advanced(state.as_ref(), &viewer, query.as_deref(), &uri).await;
    }
    Ok(
        project_list(state.as_ref(), &viewer, &key, query.as_deref(), &uri)
            .await?
            .into_response(),
    )
}

/// `GET /project/view/{id}/` and `/project/profile/{id}/`.
pub(crate) async fn by_id<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<ProjectPage>, PageError> {
    let visible = visible_by_id(state.as_ref(), &viewer, id).await?;
    render(state.as_ref(), &viewer, visible, current(&uri)).await
}

/// `GET /project/filter/{filter}/`: the list controller without a query
/// key, so the viewer's default query, as `/project/`.
pub(crate) async fn filter<S: WebState>(
    state: State<Arc<S>>,
    viewer: WebUser,
    Path(_): Path<String>,
    query: RawQuery,
    uri: Uri,
) -> Result<Page<ProjectList>, PageError> {
    list(state, viewer, query, uri).await
}
