//! `/source/<x>/branches/` and `/source/<x>/tags/`
//! (`DiffusionBranchTableController`, `DiffusionTagListController`): the
//! branches, or the tags, with their commits.

use std::collections::HashMap;

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::branch_query::{self, BranchQuery};
use phab_app_diffusion::query::Query;
use phab_app_diffusion::tags_query::TagsQuery;
use phab_core::Phid;
use serde_json::{Value, json};

use super::browse::Header;
use super::commits::{self, Commit, Identifiers};
use super::{Context, Link, PAGE_SIZE, Pager, local_name};
use crate::error::PageError;
use crate::harbormaster::{self, method_error, status};
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, time};

/// A branch or a tag of the list.
pub(crate) struct Item {
    name: String,
    href: String,
    /// The status icon and its label (branches).
    status: Option<(&'static str, &'static str)>,
    commit_name: String,
    commit_href: String,
    /// The text after the commit: a summary, or a tag's description.
    details: Option<String>,
    attributes_html: Vec<String>,
    build: Option<(status::Status, String)>,
    compare_href: Option<String>,
    browse_href: String,
}

#[derive(Template)]
#[template(path = "diffusion_refs.html")]
pub(crate) struct RefsPage {
    shell: crate::Shell,
    header: Header,
    box_title: String,
    empty: Option<(&'static str, &'static str)>,
    items: Vec<Item>,
    pager: Pager,
}

/// The commits of these hashes, and their buildables, by hash.
async fn commits_and_builds<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
    hashes: &[String],
) -> Result<(HashMap<String, Commit>, HashMap<Phid, (String, i64)>), PageError> {
    let loaded = commits::load(
        state.database(),
        context.repository.id,
        Identifiers::Exact(hashes),
    )
    .await?;
    let mut builds = HashMap::new();
    if harbormaster::can_use(state, viewer, harbormaster::APPLICATION).await? {
        let phids: Vec<Phid> = loaded.iter().map(|commit| commit.phid.clone()).collect();
        let found = phab_app_harbormaster::web::buildables_of_objects(
            &context.scope.request(),
            &phids,
            Some(false),
        )
        .await
        .map_err(method_error)?;
        for buildable in found {
            builds.insert(buildable.object_phid, (buildable.status, buildable.id));
        }
    }
    let commits = loaded
        .into_iter()
        .map(|commit| (commit.identifier.clone(), commit))
        .collect();
    Ok((commits, builds))
}

fn build_of(
    commit: Option<&Commit>,
    builds: &HashMap<Phid, (String, i64)>,
) -> Option<(status::Status, String)> {
    let (status, id) = builds.get(&commit?.phid)?;
    Some((status::buildable(status), format!("/B{id}")))
}

/// `GET /source/<x>/branches/<blob>`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn branches<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    offset: i64,
) -> Result<Response, PageError> {
    let mut params = json!({
        "offset": offset,
        "limit": PAGE_SIZE + 1,
        "branch": null,
    });
    if let Some(contains) = context
        .commit
        .as_deref()
        .filter(|commit| !commit.is_empty())
    {
        params["contains"] = json!(contains);
    }
    let found = context.call(&Query(BranchQuery), params).await?;
    let mut found: Vec<Value> = found.as_array().cloned().unwrap_or_default();
    let base = context.uri("branches", Link::default());
    let pager = Pager::slice(offset, &base, &mut found);
    let repository = &context.repository;
    let default_branch = context.default_branch();
    let mut branches: Vec<(String, String)> = found
        .iter()
        .filter_map(|branch| {
            Some((
                branch.get("shortName")?.as_str()?.to_owned(),
                branch.get("commitIdentifier")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    // shouldPublishRef(): the repository publishes, and the branch is
    // tracked and permanent.
    let publishes = !repository
        .detail("herald-disabled")
        .is_some_and(phab_core::php::is_truthy);
    let mut permanent = HashMap::new();
    for (name, _) in &branches {
        let is = publishes
            && branch_query::tracks_branch(repository, name).map_err(method_error)?
            && branch_query::is_permanent_branch(repository, name).map_err(method_error)?;
        permanent.insert(name.clone(), is);
    }
    if offset == 0 && pager.next.is_none() {
        // sortBranches(): the default branch, the permanent ones, the rest.
        branches.sort_by_key(|(name, _)| {
            (
                *name != default_branch,
                !permanent.get(name).copied().unwrap_or(false),
                name.clone(),
            )
        });
    }
    let hashes: Vec<String> = branches.iter().map(|(_, hash)| hash.clone()).collect();
    let (commits, builds) = commits_and_builds(state, viewer, &context, &hashes).await?;
    let items = branches
        .iter()
        .map(|(name, hash)| {
            let commit = commits.get(hash);
            let status = if *name == default_branch {
                ("fa-star", "Default Branch")
            } else if permanent.get(name).copied().unwrap_or(false) {
                ("fa-code-fork", "Permanent Ref")
            } else {
                ("fa-folder-open-o grey", "Not a Permanent Ref")
            };
            Item {
                name: name.clone(),
                href: context.uri(
                    "history",
                    Link {
                        branch: Some(name),
                        ..Link::default()
                    },
                ),
                status: Some(status),
                commit_name: local_name(repository, hash),
                commit_href: repository.commit_uri(hash),
                details: commit.map(|commit| commit.summary.clone()),
                attributes_html: commit
                    .map(|commit| time::format_utc(commit.epoch))
                    .into_iter()
                    .collect(),
                build: build_of(commit, &builds),
                compare_href: (*name != context.branch).then(|| compare_uri(&context, name)),
                browse_href: context.uri(
                    "browse",
                    Link {
                        branch: Some(name),
                        ..Link::default()
                    },
                ),
            }
        })
        .collect();
    let header = Header::new("Branches".to_owned(), "fa-code-fork")
        .tag(context.branch_tag())
        .tabs(context.tabs("branch"));
    Ok(Page(RefsPage {
        shell: context.shell(state, viewer, &context.title("Branches")),
        header,
        box_title: repository.name.clone(),
        empty: branches
            .is_empty()
            .then_some(("No Branches", "This repository has no branches.")),
        items,
        pager,
    })
    .into_response())
}

/// The `compare` URI with a `head`.
fn compare_uri(context: &Context, head: &str) -> String {
    context.compare_uri(Some(head), None)
}

/// `GET /source/<x>/tags/<blob>`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn tags<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    offset: i64,
) -> Result<Response, PageError> {
    let mut params = json!({"limit": PAGE_SIZE + 1, "offset": offset});
    let is_commit = context
        .commit
        .as_deref()
        .is_some_and(|commit| !commit.is_empty());
    if is_commit {
        params["commit"] = json!(context.commit);
    }
    let found = context.call(&Query(TagsQuery), params).await?;
    let mut found: Vec<Value> = found.as_array().cloned().unwrap_or_default();
    let base = context.uri("tags", Link::default());
    let pager = Pager::slice(offset, &base, &mut found);
    let repository = &context.repository;
    let text = |tag: &Value, key: &str| {
        tag.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let hashes: Vec<String> = found
        .iter()
        .map(|tag| text(tag, "commitIdentifier"))
        .collect();
    let (commits, builds) = commits_and_builds(state, viewer, &context, &hashes).await?;
    let mut phids: Vec<Phid> = commits.values().flat_map(Commit::user_phids).collect();
    phids.sort();
    phids.dedup();
    let handles = harbormaster::load_handles(state, &context.scope, &phids).await?;
    let items = found
        .iter()
        .map(|tag| {
            let name = text(tag, "name");
            let hash = text(tag, "commitIdentifier");
            let commit = commits.get(&hash);
            let description = if text(tag, "type") == "git/tag" {
                tag.get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            } else {
                commit.map_or_else(
                    || {
                        tag.get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    },
                    |commit| Some(commit.summary.clone()),
                )
            };
            let mut attributes_html = Vec::new();
            if let Some(commit) = commit {
                // renderAuthor().
                let author = commit.author().map_or_else(
                    || commits::render_name(&text(tag, "author")),
                    |person| commit.person_html(&person, &handles),
                );
                attributes_html.push(format!(
                    "<strong class=\"diffusion-history-author-name\">{author}</strong> on {}.",
                    diff_html::escape(&time::format_utc(commit.epoch))
                ));
            }
            Item {
                href: context.uri(
                    "history",
                    Link {
                        commit: Some(&name),
                        ..Link::default()
                    },
                ),
                status: None,
                commit_name: local_name(repository, &hash),
                commit_href: repository.commit_uri(&hash),
                details: description,
                attributes_html,
                build: build_of(commit, &builds),
                compare_href: Some(compare_uri(&context, &name)),
                browse_href: context.uri(
                    "browse",
                    Link {
                        commit: Some(&name),
                        ..Link::default()
                    },
                ),
                name,
            }
        })
        .collect();
    let header = Header::new("Tags".to_owned(), "fa-tags")
        .tag(context.branch_tag())
        .tabs(context.tabs("tags"));
    Ok(Page(RefsPage {
        shell: context.shell(state, viewer, &context.title("Tags")),
        header,
        box_title: repository.display_name(),
        empty: found.is_empty().then_some((
            "No Tags",
            if is_commit {
                "This commit has no tags."
            } else {
                "This repository has no tags."
            },
        )),
        items,
        pager,
    })
    .into_response())
}
