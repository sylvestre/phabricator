//! `/source/<x>/` and `/diffusion/<x>/` (`DiffusionRepositoryController`):
//! the repository, its description, the top directory of the branch with
//! its README, and the branches to switch to.
//!
//! Left to PHP: repositories still importing, and the "Locate File"
//! typeahead, which PHP's path tree endpoint serves.

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::branch_query::BranchQuery;
use phab_app_diffusion::browse_query::BrowseQuery;
use phab_app_diffusion::query::Query;
use phab_core::php as core_php;
use phab_remarkup::Mode;
use serde_json::{Value, json};

use super::browse::{self, Header, Readme, Table};
use super::{Context, Link, PAGE_SIZE, Pager, Tag};
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, remarkup};

/// `getBranchLimit()`.
const BRANCH_LIMIT: usize = 15;

#[derive(Template)]
#[template(path = "diffusion_repository.html")]
pub(crate) struct RepositoryPage {
    shell: crate::Shell,
    header: Header,
    read_only: Option<String>,
    description: Option<String>,
    branches: Option<BranchMenu>,
    actions: Vec<(&'static str, &'static str, String)>,
    clone_uri: String,
    /// The warning when there is nothing to show (`PHUIInfoView`).
    empty: Option<(&'static str, String)>,
    /// The listing's title, linked to the directory.
    listing: Option<(String, String)>,
    table: Option<Table>,
    pager: Pager,
    readme: Option<Readme>,
}

/// The branch menu: "Branch: master", the branches and their links, and
/// the "See More Branches" link.
pub(crate) struct BranchMenu {
    label: String,
    branches: Vec<(String, String)>,
    more: Option<String>,
}

/// Whether the branch has a ref cursor with positions.
async fn branch_exists<S: WebState>(state: &S, context: &Context) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} c JOIN {} p ON p.cursorID = c.id WHERE c.repositoryPHID = ? \
         AND c.refType = 'branch' AND c.refNameHash = ? LIMIT 1",
        database.table("repository", "repository_refcursor"),
        database.table("repository", "repository_refposition"),
    );
    let hash = phab_core::hash::digest_for_index(context.branch.as_bytes());
    Ok(phab_db::sqlx::query(&sql)
        .bind(context.repository.phid.as_str())
        .bind(hash)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// Whether the repository has any commit.
async fn has_commits<S: WebState>(state: &S, context: &Context) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE repositoryID = ? LIMIT 1",
        database.table("repository", "repository_commit")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(context.repository.id)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// The status tag of `buildHeaderView()`.
fn status_tag(context: &Context) -> Result<Tag, PageError> {
    let repository = &context.repository;
    let detail = |key: &str| repository.detail(key).is_some_and(core_php::is_truthy);
    let (icon, text) = if !repository.is_tracked() {
        ("fa-ban", "Inactive")
    } else if detail("read-only") {
        ("fa-wrench", "Under Maintenance")
    } else if detail("importing") {
        return Err(PageError::Fallback("a repository still importing"));
    } else if detail("herald-disabled") {
        ("fa-minus", "Publishing Disabled")
    } else {
        ("fa-check", "Active")
    };
    Ok(Tag {
        text: text.to_owned(),
        href: None,
        icon: Some(icon),
        outline: false,
    })
}

/// `buildBranchList()`.
async fn branch_list(context: &Context) -> Result<Option<BranchMenu>, PageError> {
    let branches = context
        .call(
            &Query(BranchQuery),
            json!({"closed": false, "limit": BRANCH_LIMIT + 1}),
        )
        .await?;
    let branches = branches.as_array().cloned().unwrap_or_default();
    if branches.is_empty() {
        return Ok(None);
    }
    let more = (branches.len() > BRANCH_LIMIT).then(|| context.uri("branches", Link::default()));
    let links = branches
        .iter()
        .take(BRANCH_LIMIT)
        .filter_map(|branch| branch.get("shortName").and_then(Value::as_str))
        .map(|name| {
            (
                name.to_owned(),
                context.uri(
                    "browse",
                    Link {
                        branch: Some(name),
                        ..Link::default()
                    },
                ),
            )
        })
        .collect();
    Ok(Some(BranchMenu {
        label: format!("Branch: {}", context.branch),
        branches: links,
        more,
    }))
}

/// `GET /source/<x>/` and `/source/<x>/repository/<branch>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    offset: i64,
) -> Result<Response, PageError> {
    let repository = &context.repository;
    let mut header = Header::new(diff_html::escape(&repository.name), "")
        .search(context.uri(
            "browse",
            Link {
                path: Some("/"),
                ..Link::default()
            },
        ))
        .tag(status_tag(&context)?);
    if context.default_branch() != context.branch {
        header = header.tag(context.branch_tag());
    }

    let mut empty = None;
    let has_content = branch_exists(state, &context).await?;
    if !has_content {
        empty = Some(if context.default_branch() == context.branch {
            (
                "No Default Branch",
                format!(
                    "This repository is configured with default branch \"{}\"  but there is no \
                     branch with that name in this repository.",
                    context.branch
                ),
            )
        } else {
            (
                "No Such Branch",
                format!(
                    "There is no branch named \"{}\" in this repository.",
                    context.branch
                ),
            )
        });
        if !has_commits(state, &context).await? {
            empty = Some((
                "Empty Repository",
                "This repository does not have any commits yet.".to_owned(),
            ));
        }
    }

    let (mut listing, mut table, mut pager, mut readme, mut branches) =
        (None, None, Pager::default(), None, None);
    if has_content {
        let stable = context.stable().await?;
        let result = context
            .call(
                &Query(BrowseQuery),
                json!({
                    "commit": context.commit().await?,
                    "path": context.path,
                    "limit": PAGE_SIZE + 1,
                }),
            )
            .await?;
        let mut paths: Vec<Value> = result
            .get("paths")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let browse_uri = context.uri("browse", Link::default());
        // PHP lists from the start whatever the offset, and pages to browse.
        pager = Pager::slice(offset, &browse_uri, &mut paths);
        readme = browse::readme(state, viewer, &context, &stable, &paths).await?;
        table = Some(browse::table(&context, &stable, &paths));
        listing = Some((
            format!("{} ({})", repository.name, context.branch),
            browse_uri,
        ));
        branches = branch_list(&context).await?;
        header = header.tabs(context.tabs("code"));
    }

    let description = match repository
        .detail("description")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        Some(text) => {
            let markup = remarkup::render(state, &viewer.user.viewer(), text, Mode::Html).await?;
            Some(String::from_utf8_lossy(markup.as_bytes()).into_owned())
        }
        None => None,
    };
    let read_only = repository
        .detail("read-only")
        .is_some_and(core_php::is_truthy)
        .then(|| {
            let mut message =
                "This repository is currently in read-only maintenance mode.".to_owned();
            if let Some(extra) = repository
                .detail("read-only-message")
                .and_then(Value::as_str)
            {
                message = format!("{message}\n\n{extra}");
            }
            diff_html::escape(&message).replace('\n', "<br />")
        });
    let base = repository.uri();
    let actions = vec![
        ("Manage Repository", "fa-cogs", format!("{base}manage/")),
        (
            "View Pull Logs",
            "fa-download",
            format!("/diffusion/pulllog/?repositories={}", repository.phid),
        ),
    ];
    Ok(Page(RepositoryPage {
        shell: context.shell(
            state,
            viewer,
            &format!("{} · {}", repository.name, repository.display_name()),
        ),
        header,
        read_only,
        description,
        branches,
        actions,
        clone_uri: context.uri("clone", Link::default()),
        empty,
        listing,
        table,
        pager,
        readme,
    })
    .into_response())
}
