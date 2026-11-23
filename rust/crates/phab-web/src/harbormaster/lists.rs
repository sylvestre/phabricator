//! `/harbormaster/`, `/harbormaster/build/` and `/harbormaster/plan/`:
//! the default and builtin queries of `HarbormasterBuildableSearchEngine`
//! ("All Buildables"), `HarbormasterBuildSearchEngine` ("My Builds",
//! "All Builds"...) and `HarbormasterBuildPlanSearchEngine` ("Active
//! Plans", "All Plans"). Filters and saved queries are PHP's.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_harbormaster::web as harbormaster;
use phab_core::{Phid, PhidType};
use phab_db::edges;

use super::status::{self, Status};
use super::{Scope, method_error};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

/// `PHUIHandleTagListView::setLimit()` of the plan list.
const PROJECT_LIMIT: usize = 4;

/// A buildable in the list.
pub(crate) struct BuildableItem {
    id: i64,
    header: Option<String>,
    attributes: Vec<String>,
    manual: bool,
    status: Status,
}

#[derive(Template)]
#[template(path = "buildables.html")]
pub(crate) struct BuildableList {
    shell: Shell,
    items: Vec<BuildableItem>,
    next: Option<i64>,
}

/// A plan in the list.
pub(crate) struct PlanItem {
    id: i64,
    name: String,
    disabled: bool,
    autoplan: bool,
    /// The projects' names and links, the restricted ones without.
    projects: Vec<(String, Option<String>)>,
    more_projects: bool,
}

#[derive(Template)]
#[template(path = "plans.html")]
pub(crate) struct PlanList {
    shell: Shell,
    title: &'static str,
    key: &'static str,
    items: Vec<PlanItem>,
    next: Option<i64>,
}

#[derive(Template)]
#[template(path = "builds.html")]
pub(crate) struct BuildList {
    shell: Shell,
    title: &'static str,
    key: &'static str,
    items: Vec<super::plan::BuildItem>,
    next: Option<i64>,
}

/// The builtin queries of `HarbormasterBuildSearchEngine`: key, name and
/// statuses.
const BUILD_QUERIES: &[(&str, &str, &[&str])] = &[
    ("initiated", "My Builds", &[]),
    ("all", "All Builds", &[]),
    ("waiting", "Waiting", &["inactive", "pending"]),
    ("active", "Active", &["building", "paused"]),
    (
        "completed",
        "Completed",
        &["passed", "failed", "aborted", "error", "deadlocked"],
    ),
];

/// The builtin queries of `HarbormasterBuildPlanSearchEngine`.
const PLAN_QUERIES: &[(&str, &str)] = &[("active", "Active Plans"), ("all", "All Plans")];

/// The `after` cursor of a list's query string; any other parameter is a
/// query PHP builds.
fn cursor(query: Option<&str>) -> Result<Option<i64>, PageError> {
    let mut after = None;
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match key.as_ref() {
            "after" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                after = Some(
                    value
                        .parse()
                        .map_err(|_| PageError::Fallback("an unusual cursor"))?,
                );
            }
            _ => return Err(PageError::Fallback("a search query")),
        }
    }
    Ok(after)
}

/// Whether the viewer's default query of `engine` is the first builtin
/// one, which is the only one ported.
async fn require_builtin_default<S: WebState>(
    state: &S,
    viewer: &WebUser,
    engine: &str,
) -> Result<(), PageError> {
    if harbormaster::has_named_queries(state.database(), &viewer.user.phid, engine).await? {
        return Err(PageError::Fallback("saved search queries"));
    }
    Ok(())
}

/// `getHarbormasterBuildableDisplayPHID()`: a diff shows its revision.
async fn display_phid<S: WebState>(state: &S, object: &Phid) -> Result<Phid, PageError> {
    if object.phid_type() != PhidType::DIFF {
        return Ok(object.clone());
    }
    let mut revisions =
        harbormaster::diff_revisions(state.database(), std::slice::from_ref(object)).await?;
    Ok(revisions.remove(object).unwrap_or_else(|| object.clone()))
}

/// `GET /harbormaster/`.
pub(crate) async fn buildables<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<BuildableList>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let after = cursor(query.as_deref())?;
    require_builtin_default(state.as_ref(), &viewer, "HarbormasterBuildableSearchEngine").await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let page = harbormaster::buildables_page(&scope.request(), PAGE_SIZE, after)
        .await
        .map_err(method_error)?;

    let mut displays = Vec::new();
    let mut phids = Vec::new();
    for buildable in &page.items {
        let display = display_phid(state.as_ref(), &buildable.object_phid).await?;
        phids.push(display.clone());
        phids.push(buildable.object_phid.clone());
        if let Some(container) = buildable
            .container_phid
            .as_deref()
            .filter(|phid| !phid.is_empty())
        {
            phids.push(Phid::new(container));
        }
        displays.push(display);
    }
    let handles = super::load_handles(state.as_ref(), &scope, &phids).await?;
    let items = page
        .items
        .iter()
        .zip(&displays)
        .map(|(buildable, display)| {
            let mut attributes = Vec::new();
            if let Some(container) = buildable
                .container_phid
                .as_deref()
                .filter(|phid| !phid.is_empty() && *phid != display.as_str())
            {
                attributes.push(super::handle(&handles, &Phid::new(container)).name.clone());
            }
            if buildable.object_phid != *display {
                attributes.push(
                    super::handle(&handles, &buildable.object_phid)
                        .full_name
                        .clone(),
                );
            }
            BuildableItem {
                id: buildable.id,
                header: Some(super::handle(&handles, display).full_name.clone()),
                attributes,
                manual: buildable.is_manual,
                status: status::buildable(&buildable.status),
            }
        })
        .collect();
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(BuildableList {
        shell: Shell::for_user(state.config(), &viewer, "All Buildables", current),
        items,
        next: page.next,
    }))
}

/// `GET /harbormaster/plan/`.
pub(crate) async fn plans<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<PlanList>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    require_builtin_default(state.as_ref(), &viewer, "HarbormasterBuildPlanSearchEngine").await?;
    show_plans(state, viewer, PLAN_QUERIES[0], query, uri).await
}

/// `GET /harbormaster/plan/query/<key>/`: the builtin queries.
pub(crate) async fn plan_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<PlanList>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let builtin = PLAN_QUERIES
        .iter()
        .find(|(builtin, _)| *builtin == key)
        .ok_or(PageError::Fallback("a saved plan query"))?;
    show_plans(state, viewer, *builtin, query, uri).await
}

async fn show_plans<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    (key, title): (&'static str, &'static str),
    query: Option<String>,
    uri: Uri,
) -> Result<Page<PlanList>, PageError> {
    let after = cursor(query.as_deref())?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let active = ["active".to_owned()];
    let page = harbormaster::plans_page(
        &scope.request(),
        (key == "active").then_some(&active[..]),
        PAGE_SIZE,
        after,
    )
    .await
    .map_err(method_error)?;

    let plan_phids: Vec<Phid> = page.items.iter().map(|plan| plan.phid.clone()).collect();
    let edges = edges::load_edges(
        state.database(),
        &plan_phids,
        &[edges::OBJECT_HAS_PROJECT],
        None,
    )
    .await?;
    let project_phids: Vec<Phid> = edges.iter().map(|edge| edge.dst.clone()).collect();
    let handles = super::load_handles(state.as_ref(), &scope, &project_phids).await?;
    let items = page
        .items
        .iter()
        .map(|plan| {
            let projects: Vec<(String, Option<String>)> = edges
                .iter()
                .filter(|edge| edge.src == plan.phid)
                .map(|edge| {
                    let handle = super::handle(&handles, &edge.dst);
                    (handle.name.clone(), handle.uri.clone())
                })
                .collect();
            PlanItem {
                id: plan.id,
                name: harbormaster::plan_name(plan),
                disabled: plan.status == "disabled",
                autoplan: plan.auto_key.is_some(),
                more_projects: projects.len() > PROJECT_LIMIT,
                projects: projects.into_iter().take(PROJECT_LIMIT).collect(),
            }
        })
        .collect();
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(PlanList {
        shell: Shell::for_user(state.config(), &viewer, title, current),
        title,
        key,
        items,
        next: page.next,
    }))
}

/// `GET /harbormaster/build/`: "My Builds".
pub(crate) async fn builds<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<BuildList>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    require_builtin_default(state.as_ref(), &viewer, "HarbormasterBuildSearchEngine").await?;
    show_builds(state, viewer, BUILD_QUERIES[0], query, uri).await
}

/// `GET /harbormaster/build/query/<key>/`: the builtin queries.
pub(crate) async fn build_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<BuildList>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let builtin = BUILD_QUERIES
        .iter()
        .find(|(builtin, _, _)| *builtin == key)
        .ok_or(PageError::Fallback("a saved build query"))?;
    show_builds(state, viewer, *builtin, query, uri).await
}

async fn show_builds<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    (key, title, statuses): (&'static str, &'static str, &'static [&'static str]),
    query: Option<String>,
    uri: Uri,
) -> Result<Page<BuildList>, PageError> {
    let after = cursor(query.as_deref())?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let statuses: Vec<String> = statuses.iter().map(|status| (*status).to_owned()).collect();
    let initiators = [viewer.user.phid.as_str().to_owned()];
    let page = harbormaster::builds_page(
        &scope.request(),
        (!statuses.is_empty()).then_some(&statuses[..]),
        (key == "initiated").then_some(&initiators[..]),
        PAGE_SIZE,
        after,
    )
    .await
    .map_err(method_error)?;
    let items = super::plan::build_items(state.as_ref(), &scope, &page.items).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(BuildList {
        shell: Shell::for_user(state.config(), &viewer, title, current),
        title,
        key,
        items,
        next: page.next,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_cursor_is_ported() {
        assert_eq!(cursor(None).unwrap(), None);
        assert_eq!(cursor(Some("after=12")).unwrap(), Some(12));
        assert!(cursor(Some("statuses[0]=passed")).is_err());
        assert!(cursor(Some("after=x")).is_err());
    }
}
