//! `/harbormaster/` and `/harbormaster/plan/`: the default queries of
//! `HarbormasterBuildableSearchEngine` ("All Buildables") and
//! `HarbormasterBuildPlanSearchEngine` ("Active Plans"). Other queries,
//! filters and saved queries are PHP's.

use std::sync::Arc;

use askama::Template;
use axum::extract::{RawQuery, State};
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
    items: Vec<PlanItem>,
    next: Option<i64>,
}

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
    let after = cursor(query.as_deref())?;
    require_builtin_default(state.as_ref(), &viewer, "HarbormasterBuildPlanSearchEngine").await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let page = harbormaster::plans_page(
        &scope.request(),
        Some(&["active".to_owned()]),
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
        shell: Shell::for_user(state.config(), &viewer, "Active Plans", current),
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
