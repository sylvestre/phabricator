//! `/diffusion/` and `/diffusion/query/<key>/`
//! (`DiffusionRepositoryListController`): the builtin queries of
//! `PhabricatorRepositorySearchEngine` (Active Repositories, All
//! Repositories), by the date of their latest commit. Other queries,
//! filters and saved queries are PHP's.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_differential::repository::{self, Repository};
use phab_core::Phid;
use phab_db::{edges, rows};

use super::APPLICATION;
use crate::error::PageError;
use crate::harbormaster::{self, Scope, internal};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

/// A repository of the list (`PHUIObjectItemView`).
pub(crate) struct Item {
    monogram: String,
    name: String,
    href: String,
    /// The latest commit: its link and text, and its date.
    commit: Option<(String, String, String)>,
    vcs: &'static str,
    /// The "N Commits" link, or `None` for "No Commits".
    size: Option<(String, String)>,
    projects_html: Vec<String>,
    disabled: bool,
    importing: bool,
}

#[derive(Template)]
#[template(path = "diffusion_list.html")]
pub(crate) struct ListPage {
    shell: Shell,
    query: &'static str,
    title: &'static str,
    items: Vec<Item>,
    next: Option<i64>,
}

/// `PhabricatorRepositoryType::getNameForRepositoryType()`.
pub(crate) fn vcs_name(vcs: &str) -> &'static str {
    match vcs {
        "git" => "Git",
        "hg" => "Mercurial",
        "svn" => "Subversion",
        _ => "Unknown",
    }
}

/// `PhutilNumber`: digits grouped by thousands.
pub(crate) fn thousands(number: i64) -> String {
    let digits = number.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if number < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// The `after` cursor of the query string; any other parameter is a query
/// PHP builds.
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

/// A `repository_summary` row.
#[derive(Clone, Copy)]
struct Summary {
    size: i64,
    last_commit: Option<i64>,
    epoch: Option<i64>,
}

/// The repositories in the `committed` order (latest commit first, those
/// without commits last, then by ID), with their summaries.
async fn ordered<S: WebState>(state: &S) -> Result<Vec<(i64, Summary)>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT r.id, s.size, s.lastCommitID, s.epoch FROM {} r LEFT JOIN {} s \
         ON r.id = s.repositoryID",
        database.table("repository", "repository"),
        database.table("repository", "repository_summary"),
    );
    let found = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let mut ordered = Vec::new();
    for row in &found {
        ordered.push((
            rows::int(row, "id")?,
            Summary {
                size: rows::opt_int(row, "size")?.unwrap_or(0),
                last_commit: rows::opt_int(row, "lastCommitID")?.filter(|id| *id != 0),
                epoch: rows::opt_int(row, "epoch")?,
            },
        ));
    }
    ordered.sort_by_key(|(id, summary)| {
        (
            summary.epoch.is_none(),
            std::cmp::Reverse(summary.epoch),
            std::cmp::Reverse(*id),
        )
    });
    Ok(ordered)
}

/// The latest commits: hash, summary and repository, by ID.
async fn latest_commits<S: WebState>(
    state: &S,
    ids: &[i64],
) -> Result<HashMap<i64, (i64, String, String)>, PageError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let database = state.database();
    let sql = format!(
        "SELECT id, repositoryID, commitIdentifier, summary FROM {} WHERE id IN ({})",
        database.table("repository", "repository_commit"),
        phab_db::placeholders(ids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let mut commits = HashMap::new();
    for row in &found {
        commits.insert(
            rows::int(row, "id")?,
            (
                rows::int(row, "repositoryID")?,
                rows::text(row, "commitIdentifier")?,
                rows::text(row, "summary")?,
            ),
        );
    }
    Ok(commits)
}

#[allow(clippy::too_many_lines)]
async fn list_page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<ListPage>, PageError> {
    let after = cursor(query)?;
    let database = state.database();
    let mut ordered = ordered(state).await?;
    if let Some(after) = after {
        let Some(position) = ordered.iter().position(|(id, _)| *id == after) else {
            return Err(PageError::Fallback("a cursor which doesn't exist"));
        };
        ordered.drain(..=position);
    }
    let ids: Vec<i64> = ordered.iter().map(|(id, _)| *id).collect();
    let loaded: HashMap<i64, Repository> = repository::load_by_ids(database, &ids)
        .await?
        .into_iter()
        .map(|repository| (repository.id, repository))
        .collect();
    let visible = repository::visible(
        database,
        state.config(),
        &viewer.user.viewer(),
        loaded.into_values().collect(),
        state.now(),
    )
    .await
    .map_err(internal)?;
    let mut visible: HashMap<i64, Repository> = visible
        .into_iter()
        .map(|repository| (repository.id, repository))
        .collect();
    // willFilterPage(): the status of "active".
    let mut shown: Vec<(Repository, Summary)> = ordered
        .iter()
        .filter_map(|(id, summary)| Some((visible.remove(id)?, *summary)))
        .filter(|(repository, _)| key != "active" || repository.is_tracked())
        .collect();
    let next = (shown.len() > PAGE_SIZE).then(|| shown[PAGE_SIZE - 1].0.id);
    shown.truncate(PAGE_SIZE);

    let commit_ids: Vec<i64> = shown
        .iter()
        .filter_map(|(_, summary)| summary.last_commit)
        .collect();
    let commits = latest_commits(state, &commit_ids).await?;
    let phids: Vec<Phid> = shown
        .iter()
        .map(|(repository, _)| repository.phid.clone())
        .collect();
    let project_edges =
        edges::load_edges(database, &phids, &[edges::OBJECT_HAS_PROJECT], None).await?;
    let project_phids: Vec<Phid> = project_edges.iter().map(|edge| edge.dst.clone()).collect();
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &project_phids).await?;

    let items = shown
        .iter()
        .map(|(repository, summary)| {
            let commit = summary
                .last_commit
                .and_then(|id| commits.get(&id))
                .filter(|(repository_id, ..)| *repository_id == repository.id)
                .map(|(_, identifier, summary_text)| {
                    (
                        repository.commit_uri(identifier),
                        format!(
                            "{}: {summary_text}",
                            super::local_name(repository, identifier)
                        ),
                        summary.epoch.map(time::format_utc).unwrap_or_default(),
                    )
                });
            let size = (summary.size > 0).then(|| {
                let label = if summary.size == 1 {
                    "1 Commit".to_owned()
                } else {
                    format!("{} Commits", thousands(summary.size))
                };
                (
                    super::uri::generate(repository, "history", super::uri::Target::default()),
                    label,
                )
            });
            let projects_html = project_edges
                .iter()
                .filter(|edge| edge.src == repository.phid)
                .filter_map(|edge| handles.get(&edge.dst))
                .map(harbormaster::Handle::short_link_html)
                .collect();
            Item {
                monogram: repository.monogram(),
                name: repository.name.clone(),
                href: repository.uri(),
                commit,
                vcs: vcs_name(&repository.vcs),
                size,
                projects_html,
                disabled: !repository.is_tracked(),
                importing: repository
                    .detail("importing")
                    .is_some_and(phab_core::php::is_truthy),
            }
        })
        .collect();
    let title = if key == "active" {
        "Active Repositories"
    } else {
        "All Repositories"
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(ListPage {
        shell: Shell::for_user(state.config(), viewer, &format!("Query: {title}"), current),
        query: key,
        title,
        items,
        next,
    }))
}

/// `GET /diffusion/`: the viewer's default query, "Active Repositories"
/// unless they saved or reordered queries.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ListPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if phab_app_harbormaster::web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        "PhabricatorRepositorySearchEngine",
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    list_page(state.as_ref(), &viewer, "active", query.as_deref(), &uri).await
}

/// `GET /diffusion/query/{key}/`: a builtin query.
pub(crate) async fn list_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<ListPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "active" => "active",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved search query")),
    };
    list_page(state.as_ref(), &viewer, key, query.as_deref(), &uri).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_numbers() {
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn only_the_cursor_is_ported() {
        assert_eq!(cursor(Some("after=5")).unwrap(), Some(5));
        assert!(cursor(Some("status=open")).is_err());
    }
}
