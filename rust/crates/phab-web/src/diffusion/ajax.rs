//! The endpoints Diffusion pages load with scripts, which answer with
//! `AphrontAjaxResponse`: the last change of each entry of a directory
//! (`lastmodified/`, `DiffusionLastModifiedController`), the branches and
//! tags of a commit (`commit/<hash>/branches/` and `/tags/`), the blame of
//! a file (`blame/`, `DiffusionBlameController`) and the paths of the
//! "Locate File" typeahead (`pathtree/`, `DiffusionPathTreeController`).
//!
//! They answer PHP's pages as well as Rust's. PHP's tooltips are `title`
//! attributes here, and blame shows no profile pictures.

use std::collections::HashMap;
use std::fmt::Write as _;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use phab_app_diffusion::blame::Blame;
use phab_app_diffusion::branch_query::BranchQuery;
use phab_app_diffusion::last_modified_query::LastModifiedQuery;
use phab_app_diffusion::query::Query;
use phab_app_diffusion::query_paths::QueryPaths;
use phab_app_diffusion::tags_query::TagsQuery;
use phab_core::Phid;
use phab_db::edges;
use serde_json::{Map, Value, json};

use super::commits::{self, Commit, Identifiers};
use super::{Context, Link};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, internal};
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// `AphrontAjaxResponse`: the payload in PHP's envelope, behind its JSON
/// shield, with `<` and `>` escaped as `encodeJSONForHTTPResponse()` does.
pub(crate) fn response(payload: &Value) -> Response {
    let envelope = json!({"error": null, "payload": payload}).to_string();
    let body = format!(
        "for (;;);{}",
        envelope.replace('<', "\\u003c").replace('>', "\\u003e")
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/plain; charset=UTF-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::X_FRAME_OPTIONS, "Deny"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// A link, escaped.
fn link(href: &str, text: &str) -> String {
    format!("<a href=\"{}\">{}</a>", escape(href), escape(text))
}

/// The `paths` of a `lastmodified/` request: a JSON list of paths.
pub(crate) fn parse_paths(paths: Option<&str>) -> Result<Vec<String>, PageError> {
    let value: Value = serde_json::from_str(paths.unwrap_or_default())
        .map_err(|_| PageError::BadRequest("The paths are not valid JSON."))?;
    let Value::Array(list) = value else {
        return Err(PageError::Fallback("paths which are not a list"));
    };
    list.into_iter()
        .map(|path| match path {
            Value::String(path) => Ok(path),
            _ => Err(PageError::Fallback("paths which are not strings")),
        })
        .collect()
}

/// `POST /source/<x>/lastmodified/<blob>`: the commit, date and summary of
/// the last change of each path.
pub(crate) async fn last_modified<S: WebState>(
    state: &S,
    context: &Context,
    paths: &[String],
) -> Result<Response, PageError> {
    if paths.is_empty() {
        // `diffusion.lastmodifiedquery` fails without paths.
        return Err(PageError::Fallback("a request without paths"));
    }
    if super::browse::has_lint(state, context).await? {
        return Err(PageError::Fallback("a branch with lint messages"));
    }
    let commit = context.commit().await?;
    let mut map = Map::new();
    for path in paths {
        map.insert(path.clone(), json!(commit));
    }
    let modified = context
        .call(&Query(LastModifiedQuery), json!({"paths": map}))
        .await?;
    let modified = modified.as_object().cloned().unwrap_or_default();
    let mut hashes: Vec<String> = modified
        .values()
        .filter_map(Value::as_str)
        .filter(|hash| !hash.is_empty())
        .map(str::to_owned)
        .collect();
    hashes.sort();
    hashes.dedup();
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
    let repository = &context.repository;
    let mut output = Map::new();
    for path in paths {
        let Some(commit) = modified
            .get(path)
            .and_then(Value::as_str)
            .and_then(|hash| by_hash.get(hash))
        else {
            continue;
        };
        let uri = repository.commit_uri(&commit.identifier);
        output.insert(
            path.clone(),
            json!({
                "commit": link(&uri, &repository.format_commit_name(&commit.identifier)),
                "date": escape(&time::format_utc(commit.epoch)),
                "details": format!(
                    "<div class=\"single-display-line-bounds\"><span \
                     class=\"single-display-line-content\">{}</span>\u{a0}</div>",
                    link(&uri, &commit.summary)
                ),
            }),
        );
    }
    Ok(response(&Value::Object(output)))
}

/// `GET /source/<x>/commit/<hash>/branches/`: the first ten branches which
/// contain the commit.
pub(crate) async fn commit_branches(context: &Context) -> Result<Response, PageError> {
    const LIMIT: usize = 10;
    let found = context
        .call(
            &Query(BranchQuery),
            json!({"contains": context.commit, "limit": LIMIT + 1, "branch": null}),
        )
        .await?;
    let found = found.as_array().cloned().unwrap_or_default();
    let mut links: Vec<String> = found
        .iter()
        .take(LIMIT)
        .filter_map(|branch| branch.get("shortName")?.as_str().map(str::to_owned))
        .map(|name| {
            let href = context.uri(
                "browse",
                Link {
                    branch: Some(&name),
                    ..Link::default()
                },
            );
            link(&href, &name)
        })
        .collect();
    if found.len() > LIMIT {
        links.push(link(
            &context.uri("branches", Link::default()),
            "More Branches…",
        ));
    }
    Ok(response(&json!(if links.is_empty() {
        "None".to_owned()
    } else {
        links.join(", ")
    })))
}

/// `GET /source/<x>/commit/<hash>/tags/`: the first ten tags of the
/// commit.
pub(crate) async fn commit_tags(context: &Context) -> Result<Response, PageError> {
    const LIMIT: usize = 10;
    let found = context
        .call(
            &Query(TagsQuery),
            json!({"commit": context.commit, "limit": LIMIT + 1}),
        )
        .await?;
    let found = found.as_array().cloned().unwrap_or_default();
    let mut links: Vec<String> = found
        .iter()
        .take(LIMIT)
        .filter_map(|tag| tag.get("name")?.as_str().map(str::to_owned))
        .map(|name| {
            let href = context.uri(
                "browse",
                Link {
                    commit: Some(&name),
                    ..Link::default()
                },
            );
            link(&href, &name)
        })
        .collect();
    if found.len() > LIMIT {
        links.push(link(&context.uri("tags", Link::default()), "More Tags…"));
    }
    Ok(response(&json!(if links.is_empty() {
        "None".to_owned()
    } else {
        links.join(", ")
    })))
}

/// `GET /source/<x>/pathtree/<blob>`: the paths at the commit, as a tree
/// of path parts (directories keep their slash), leaves being `1`.
pub(crate) async fn path_tree(context: &Context) -> Result<Response, PageError> {
    let commit = context.commit().await?;
    let paths = context
        .call(
            &Query(QueryPaths),
            json!({"path": context.path, "commit": commit}),
        )
        .await?;
    // PHP's empty array is a JSON list.
    let mut tree = json!([]);
    for path in paths.as_array().into_iter().flatten() {
        let Some(path) = path.as_str() else {
            continue;
        };
        insert_path(&mut tree, path);
    }
    Ok(response(&json!({"tree": tree})))
}

/// Adds the parts of a path (`preg_split('((?<=/))')`) to the tree.
fn insert_path(tree: &mut Value, path: &str) {
    let mut cursor = tree;
    for part in path.split_inclusive('/') {
        if !cursor.is_object() {
            *cursor = Value::Object(Map::new());
        }
        let Value::Object(map) = cursor else {
            return;
        };
        cursor = map.entry(part.to_owned()).or_insert_with(|| json!(1));
    }
}

/// What blame shows of a commit: the links, and when it was made.
pub(crate) struct BlameCommit {
    pub(crate) skip_html: String,
    pub(crate) info_html: String,
    pub(crate) epoch: i64,
}

/// `diffusion.blame` of the page's path, and what to show of each commit
/// (`DiffusionBlameController`), by identifier, in the order lines name
/// them.
#[allow(clippy::too_many_lines)]
pub(crate) async fn blame_map<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
) -> Result<(Vec<String>, Vec<(String, BlameCommit)>), PageError> {
    let commit = context.commit().await?;
    let result = context
        .call(
            &Query(Blame),
            json!({"commit": commit, "paths": [context.path]}),
        )
        .await?;
    let lines: Vec<String> = result
        .get(&context.path)
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    if lines.is_empty() {
        // PHP fails on min() of no epochs.
        return Err(PageError::Fallback("a path Git doesn't blame"));
    }
    let mut identifiers: Vec<String> = Vec::new();
    for line in &lines {
        if !identifiers.contains(line) {
            identifiers.push(line.clone());
        }
    }
    let database = state.database();
    let loaded = commits::load(
        database,
        context.repository.id,
        Identifiers::Exact(&identifiers),
    )
    .await?;
    let by_hash: HashMap<&str, &Commit> = loaded
        .iter()
        .map(|commit| (commit.identifier.as_str(), commit))
        .collect();

    // loadRevisionMapForCommits(): one revision of each commit.
    let commit_phids: Vec<Phid> = loaded.iter().map(|commit| commit.phid.clone()).collect();
    let revision_edges =
        edges::load_edges(database, &commit_phids, &[edges::COMMIT_HAS_REVISION], None).await?;
    let revision_phids: Vec<Phid> = revision_edges.iter().map(|edge| edge.dst.clone()).collect();
    let revisions =
        phab_app_differential::storage::load_revisions(database, &revision_phids).await?;
    let revisions = phab_app_differential::storage::visible_revisions(
        database,
        state.config(),
        &viewer.user.viewer(),
        revisions,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let revisions: HashMap<&Phid, _> = revisions
        .iter()
        .map(|revision| (&revision.phid, revision))
        .collect();
    let revision_of = |commit: &Commit| {
        revision_edges
            .iter()
            .filter(|edge| edge.src == commit.phid)
            .find_map(|edge| revisions.get(&edge.dst).copied())
    };

    let mut phids: Vec<Phid> = loaded
        .iter()
        .filter_map(Commit::author_display_phid)
        .filter(|phid| phid.phid_type() == phab_core::PhidType::USER)
        .collect();
    phids.extend(
        revisions
            .values()
            .filter_map(|revision| revision.author_phid.clone()),
    );
    let handles = harbormaster::load_handles(state, &context.scope, &phids).await?;
    let name_of = |phid: &Phid| handles.get(phid).map(|handle| handle.name.clone());

    let base = context.uri(
        "browse",
        Link {
            commit: Some(&context.stable().await?.hash),
            ..Link::default()
        },
    );
    let mut map = Vec::new();
    for identifier in identifiers {
        let commit = by_hash.get(identifier.as_str()).copied();
        let skip_html = format!(
            "<a href=\"{}?before={}\" title=\"Skip Past This Commit\"><span \
             class=\"visual-only phui-icon-view phui-font-fa fa fa-backward\" \
             aria-hidden=\"true\"></span></a>",
            escape(&base),
            escape(&identifier)
        );
        let mut info_html = String::new();
        match commit.and_then(Commit::author_display_phid) {
            Some(author) => {
                let name = name_of(&author).unwrap_or_default();
                match handles.get(&author).and_then(|handle| handle.uri.clone()) {
                    Some(uri) => {
                        let _ = write!(
                            info_html,
                            "<a class=\"phabricator-source-blame-author\" href=\"{}\" \
                             title=\"{}\"></a>",
                            escape(&uri),
                            escape(&name)
                        );
                    }
                    None => {
                        let _ = write!(
                            info_html,
                            "<span class=\"phabricator-source-blame-author\" title=\"{}\"></span>",
                            escape(&name)
                        );
                    }
                }
            }
            None => info_html.push_str("<span class=\"phabricator-source-blame-author\"></span>"),
        }
        if let Some(commit) = commit {
            let date = &time::format_utc(commit.epoch)[..10];
            let tip = match commit.author_display_phid().and_then(|phid| name_of(&phid)) {
                Some(author) => format!("{}\n{date} · {author}", commit.summary.trim()),
                None => format!("{}\n{date}", commit.summary.trim()),
            };
            let _ = write!(
                info_html,
                "<a href=\"{}\" title=\"{}\">{}</a>",
                escape(&context.repository.commit_uri(&commit.identifier)),
                escape(&tip),
                escape(&super::local_name(&context.repository, &commit.identifier))
            );
            if let Some(revision) = revision_of(commit) {
                let author = revision
                    .author_phid
                    .as_ref()
                    .and_then(name_of)
                    .unwrap_or_default();
                let tip = format!(
                    "D{} {}\n{} · {author}",
                    revision.id,
                    revision.title,
                    &time::format_utc(revision.date_modified)[..10]
                );
                let _ = write!(
                    info_html,
                    " · <a href=\"/D{}\" title=\"{}\">D{}</a>",
                    revision.id,
                    escape(&tip),
                    revision.id
                );
            }
        }
        map.push((
            identifier,
            BlameCommit {
                skip_html,
                info_html,
                epoch: commit.map_or(0, |commit| commit.epoch),
            },
        ));
    }
    Ok((lines, map))
}

/// `GET /source/<x>/blame/<blob>`: the commit of each line, and what to
/// show of each commit.
pub(crate) async fn blame<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
) -> Result<Response, PageError> {
    let (lines, commits) = blame_map(state, viewer, context).await?;
    let epochs: Vec<i64> = commits.iter().map(|(_, commit)| commit.epoch).collect();
    let mut map = Map::new();
    for (identifier, commit) in commits {
        map.insert(
            identifier,
            json!({
                "skip": commit.skip_html,
                "info": commit.info_html,
                "epoch": commit.epoch,
            }),
        );
    }
    Ok(response(&json!({
        "blame": lines,
        "map": map,
        "epoch": {
            "min": epochs.iter().min(),
            "max": epochs.iter().max(),
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_path_trees_like_php() {
        let mut tree = json!([]);
        for path in ["README", "dir/a.txt", "dir/b.txt", "dir/sub/c"] {
            insert_path(&mut tree, path);
        }
        assert_eq!(
            tree,
            json!({"README": 1, "dir/": {"a.txt": 1, "b.txt": 1, "sub/": {"c": 1}}})
        );
    }

    #[test]
    fn shields_json() {
        let response = response(&json!("<a>"));
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=UTF-8"
        );
    }
}
