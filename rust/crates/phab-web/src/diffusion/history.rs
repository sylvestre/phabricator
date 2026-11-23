//! `/source/<x>/history/<branch>/<path>` (`DiffusionHistoryController`):
//! the commits of a path, as `DiffusionCommitGraphView` lists them, with
//! the graph of the commits next to the history of a whole branch.

use std::collections::HashMap;

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::history_query::HistoryQuery;
use phab_app_diffusion::query::Query;
use phab_core::Phid;
use phab_db::edges;
use serde_json::{Value, json};

use super::browse::Header;
use super::commits::{self, Commit, Identifiers};
use super::{Context, Link, PAGE_SIZE, Pager, local_name};
use crate::error::PageError;
use crate::harbormaster::{self, Scope, method_error, status};
use crate::response::Page;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// A commit of the list (`PHUIObjectItemView`).
pub(crate) struct Item {
    /// The date header, on the first commit of each day.
    pub(crate) date: Option<String>,
    pub(crate) name: String,
    pub(crate) href: String,
    pub(crate) description_html: String,
    pub(crate) disabled: bool,
    pub(crate) browse_href: String,
    /// The buildable status, and the buildable's link.
    pub(crate) build: (status::Status, Option<String>),
    /// The audit status, its icon and color; `None` without audits.
    pub(crate) audit: Option<(&'static str, &'static str, &'static str)>,
    pub(crate) author_html: Option<String>,
    pub(crate) revisions_html: Vec<String>,
    /// The row of the commit graph, as JSON for the script.
    pub(crate) graph: Option<String>,
}

#[derive(Template)]
#[template(path = "diffusion_history.html")]
pub(crate) struct HistoryPage {
    shell: crate::Shell,
    header: Header,
    items: Vec<Item>,
    /// The width of the graph, in columns.
    graph_count: usize,
    pager: Pager,
}

/// `DiffusionCommitGraphView::render()`: the rows of the graph next to
/// the items; the parents of the commit after the page draw the last row.
pub(crate) fn attach_graph(
    items: &mut [Item],
    mut parents: Vec<(String, Vec<String>)>,
    filter: bool,
    pager: &Pager,
) -> usize {
    if parents.is_empty() {
        return 0;
    }
    if filter {
        super::graph::filter_parents(&mut parents);
    }
    let (rows, count) =
        super::graph::render(&parents, pager.previous.is_none(), pager.next.is_none());
    for (item, row) in items.iter_mut().zip(rows) {
        item.graph = Some(row.json());
    }
    count
}

/// The buildables of `commits`, by commit: the last of each, as
/// `mpull($buildables, null, 'getBuildablePHID')` keeps it.
async fn buildables<S: WebState>(
    state: &S,
    viewer: &WebUser,
    scope: &Scope,
    commits: &[&Commit],
) -> Result<Option<HashMap<Phid, (String, i64)>>, PageError> {
    if !harbormaster::can_use(state, viewer, harbormaster::APPLICATION).await? {
        return Ok(None);
    }
    let phids: Vec<Phid> = commits.iter().map(|commit| commit.phid.clone()).collect();
    let found =
        phab_app_harbormaster::web::buildables_of_objects(&scope.request(), &phids, Some(false))
            .await
            .map_err(method_error)?;
    let mut by_commit = HashMap::new();
    for buildable in found {
        by_commit.insert(buildable.object_phid, (buildable.status, buildable.id));
    }
    Ok(Some(by_commit))
}

/// `DiffusionCommitGraphView::newObjectItemViews()` for these commits
/// (hash and epoch, newest first), at `path`.
pub(crate) async fn items<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
    history: &[(String, Option<i64>)],
) -> Result<Vec<Item>, PageError> {
    let hashes: Vec<String> = history.iter().map(|(hash, _)| hash.clone()).collect();
    let loaded = commits::load(
        state.database(),
        context.repository.id,
        Identifiers::Exact(&hashes),
    )
    .await?;
    let by_hash: HashMap<&str, &Commit> = loaded
        .iter()
        .map(|commit| (commit.identifier.as_str(), commit))
        .collect();
    let found: Vec<&Commit> = loaded.iter().collect();
    let commit_phids: Vec<Phid> = found.iter().map(|commit| commit.phid.clone()).collect();

    let show_revisions =
        harbormaster::can_use(state, viewer, phab_policy::application::DIFFERENTIAL).await?;
    let revision_edges = if show_revisions {
        edges::load_edges(
            state.database(),
            &commit_phids,
            &[edges::COMMIT_HAS_REVISION],
            None,
        )
        .await?
    } else {
        Vec::new()
    };
    let mut phids: Vec<Phid> = revision_edges.iter().map(|edge| edge.dst.clone()).collect();
    for commit in &found {
        phids.extend(commit.user_phids());
    }
    let scope = &context.scope;
    let handles = harbormaster::load_handles(state, scope, &phids).await?;
    let builds = buildables(state, viewer, scope, &found).await?;

    let mut last_date = None;
    let mut items = Vec::new();
    for (hash, epoch) in history {
        let commit = by_hash.get(hash.as_str()).copied();
        let epoch = epoch.or(commit.map(|commit| commit.epoch));
        let date = epoch.map(|epoch| time::format_utc(epoch)[..10].to_owned());
        let header = if date == last_date {
            None
        } else {
            last_date.clone_from(&date);
            date
        };
        let build = match (&builds, commit) {
            (Some(builds), Some(commit)) => builds
                .get(&commit.phid)
                .map(|(status, id)| (status::buildable(status), Some(format!("/B{id}")))),
            _ => None,
        }
        .unwrap_or_else(|| (status::no_builds(), None));
        let revisions_html = commit
            .map(|commit| {
                revision_edges
                    .iter()
                    .filter(|edge| edge.src == commit.phid)
                    .filter_map(|edge| handles.get(&edge.dst))
                    .filter(|handle| handle.uri.is_some())
                    .map(harbormaster::Handle::link_html)
                    .collect()
            })
            .unwrap_or_default();
        items.push(Item {
            date: header,
            name: local_name(&context.repository, hash),
            href: context.repository.commit_uri(hash),
            description_html: commit.map_or_else(
                || "<em>Discovering…</em>".to_owned(),
                Commit::description_html,
            ),
            disabled: commit.is_none_or(|commit| commit.has_flags(commits::IMPORTED_UNREACHABLE)),
            browse_href: context.uri(
                "browse",
                Link {
                    commit: Some(hash),
                    ..Link::default()
                },
            ),
            build,
            audit: commit
                .filter(|commit| commit.audit_status != "none")
                .map(|commit| commits::audit_status(&commit.audit_status)),
            author_html: commit.and_then(|commit| {
                commit
                    .author()
                    .map(|person| commit.person_html(&person, &handles))
            }),
            revisions_html,
            graph: None,
        });
    }
    Ok(items)
}

/// `GET /source/<x>/history/<blob>`.
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    offset: i64,
) -> Result<Response, PageError> {
    let result = context
        .call(
            &Query(HistoryQuery),
            json!({
                "commit": context.commit().await?,
                "path": context.path,
                "offset": offset,
                "limit": PAGE_SIZE + 1,
            }),
        )
        .await?;
    let mut history: Vec<(String, Option<i64>)> = result
        .get("pathChanges")
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| {
                    let hash = change.get("commitIdentifier")?.as_str()?.to_owned();
                    let epoch = change
                        .get("commit")
                        .and_then(|commit| commit.get("epoch"))
                        .and_then(Value::as_str)
                        .and_then(|epoch| epoch.parse().ok());
                    Some((hash, epoch))
                })
                .collect()
        })
        .unwrap_or_default();
    let base = context.uri("history", Link::default());
    let pager = Pager::slice(offset, &base, &mut history);
    let mut items = items(state, viewer, &context, &history).await?;
    // Commits of a path skip others, which would make the graph wrong.
    let graph_count = if context.path.is_empty() {
        attach_graph(&mut items, super::graph::parents_of(&result), false, &pager)
    } else {
        0
    };

    let title_html = if context.path.is_empty() {
        "History".to_owned()
    } else {
        context.path_links_html("history")
    };
    let mut header = Header::new(title_html, "fa-clock-o").tag(context.branch_tag());
    if let Some(tag) = context.symbolic_tag() {
        header = header.tag(tag);
    }
    let header = header.tabs(context.tabs("history"));
    Ok(Page(HistoryPage {
        shell: context.shell(state, viewer, &context.title("History")),
        header,
        items,
        graph_count,
        pager,
    })
    .into_response())
}
