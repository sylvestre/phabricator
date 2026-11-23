//! `/rXYZ<hash>` and `/R<id>:<hash>` (`DiffusionCommitController`): a
//! commit, its details, timeline, merged changes and diff. Each file's
//! diff is PHP's `diffusion.diffquery` for the path (`git diff` with full
//! context), rendered like the revision page's changesets.
//!
//! Left to PHP: ambiguous hashes, commits not imported or rewritten,
//! unpublished commits, commits with audits, comments or inline comments
//! (the audit actions are PHP's), commits of more than 100 files, binary
//! and image changes, `?diff=` (the raw diff) and Bugtraq links.

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::commit_parents_query::CommitParentsQuery;
use phab_app_diffusion::merged_commits_query::MergedCommitsQuery;
use phab_app_diffusion::query::Query;
use phab_app_diffusion::refs_query::RefsQuery;
use phab_core::Phid;
use phab_db::{edges, rows};
use phab_diff::{ChangeType, Changeset, FileType};
use phab_remarkup::Mode;
use serde_json::{Value, json};

use super::commits::{self, Commit, Identifiers};
use super::history::{self, Item};
use super::{Context, Link, Prefix, local_name};
use crate::error::PageError;
use crate::harbormaster::{self, Handle, method_error};
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, remarkup, time};

/// `DiffusionCommitController::CHANGES_LIMIT`: above it, PHP loads the
/// files one by one.
const CHANGES_LIMIT: usize = 100;
/// `getMergeDisplayLimit()`.
const MERGE_LIMIT: usize = 50;
/// `ArcanistGitAPI::GIT_MAGIC_ROOT_COMMIT`: the empty tree.
pub(crate) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// `DiffusionCommitRevertsCommitEdgeType`.
const COMMIT_REVERTS_COMMIT: i64 = 55;
/// `DiffusionCommitRevertedByCommitEdgeType`.
const COMMIT_REVERTED_BY_COMMIT: i64 = 56;

/// A file of the table of contents.
pub(crate) struct Toc {
    path: String,
    change: &'static str,
    history_href: String,
    browse_href: String,
}

/// A timeline entry.
pub(crate) struct Entry {
    title_html: String,
    date: String,
}

#[derive(Template)]
#[template(path = "diffusion_commit.html")]
pub(crate) struct CommitPage {
    shell: crate::Shell,
    title: String,
    name: String,
    commit_href: String,
    edit_href: String,
    raw_diff_href: String,
    message_html: String,
    /// The provenance: who, and what they did when.
    provenance: Vec<(String, String)>,
    /// Further properties, as HTML.
    properties: Vec<(&'static str, String)>,
    /// Where PHP's scripts get the branches and tags.
    branches_uri: Option<String>,
    tags_uri: Option<String>,
    timeline: Vec<Entry>,
    merges: Vec<Item>,
    merges_limited: bool,
    /// A notice instead of the changes.
    info: Option<(&'static str, &'static str)>,
    toc: Vec<Toc>,
    changesets: Vec<String>,
}

/// The repository and hash of `rXYZabc` or `R12:abc`.
pub(crate) fn parse_monogram(monogram: &str) -> Option<(String, String)> {
    let is_hash = |hash: &str| {
        !hash.is_empty()
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if let Some(rest) = monogram.strip_prefix('r') {
        let split = rest
            .find(|character: char| !character.is_ascii_uppercase())
            .unwrap_or(rest.len());
        let (callsign, hash) = rest.split_at(split);
        return (!callsign.is_empty() && is_hash(hash))
            .then(|| (callsign.to_owned(), hash.to_owned()));
    }
    let rest = monogram.strip_prefix('R')?;
    let (id, hash) = rest.split_once(':')?;
    (id.starts_with(|first: char| ('1'..='9').contains(&first))
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && is_hash(hash))
    .then(|| (id.to_owned(), hash.to_owned()))
}

/// `GET /rXYZ<hash>`, routed through the monogram route.
pub(crate) async fn show<S: WebState>(
    State(state): State<std::sync::Arc<S>>,
    viewer: WebUser,
    Path(monogram): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    let (identifier, hash) = parse_monogram(&monogram).ok_or(PageError::NotFound)?;
    if super::is_vcs_request(&headers) {
        return Err(PageError::Fallback(
            "a request from a version control client",
        ));
    }
    super::offset(query.as_deref())?;
    let context =
        match Context::load(&*state, &viewer, Prefix::Commit, &identifier, None, &uri).await {
            Ok(context) => context,
            Err(response) => return Ok(*response),
        };
    let canonical = context.repository.commit_uri(&hash);
    if canonical != uri.path() {
        return Ok(super::redirect(&canonical));
    }
    page(&*state, &viewer, context, hash).await
}

/// Whether the commit has audit requests.
async fn has_audit_requests<S: WebState>(state: &S, commit: &Commit) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE commitPHID = ? LIMIT 1",
        database.table("repository", "repository_auditrequest")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(commit.phid.as_str())
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// The timeline: the "committed" transaction of the publish worker. Any
/// other transaction (comments, audit actions, inline comments...) is
/// PHP's to show.
async fn timeline<S: WebState>(
    state: &S,
    context: &Context,
    commit: &Commit,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT transactionType, newValue, dateCreated, commentPHID FROM {} \
         WHERE objectPHID = ? ORDER BY id",
        database.table("audit", "audit_transaction")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(commit.phid.as_str())
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let mut transactions = Vec::new();
    let mut phids = Vec::new();
    for row in &found {
        if rows::text(row, "transactionType")? != "audit:commit"
            || rows::opt_text(row, "commentPHID")?.is_some()
        {
            return Err(PageError::Fallback(
                "a commit with comments or audit actions",
            ));
        }
        let value = rows::json(row, "newValue")?;
        for key in ["authorPHID", "committerPHID"] {
            if let Some(phid) = value
                .get(key)
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            {
                phids.push(Phid::new(phid));
            }
        }
        transactions.push((value, rows::int(row, "dateCreated")?));
    }
    let phids: Vec<Phid> = phids
        .into_iter()
        .filter(|phid| phid.phid_type() == phab_core::PhidType::USER)
        .collect();
    let handles = harbormaster::load_handles(state, &context.scope, &phids).await?;
    let commit_link = format!(
        "<a href=\"{}\">{}</a>",
        diff_html::escape(&context.repository.commit_uri(&commit.identifier)),
        diff_html::escape(&context.repository.format_commit_name(&commit.identifier))
    );
    let person = |value: &Value, phid: &str, name: &str| -> Option<String> {
        if let Some(phid) = value
            .get(phid)
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        {
            return Some(
                handles
                    .get(&Phid::new(phid))
                    .map_or_else(|| "Unknown Object".to_owned(), Handle::link_html),
            );
        }
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(diff_html::escape)
    };
    Ok(transactions
        .iter()
        .map(|(value, date)| {
            let mut author = person(value, "authorPHID", "authorName");
            let committer =
                person(value, "committerPHID", "committerName").or_else(|| author.take());
            let committer = committer.unwrap_or_default();
            let title_html = match author {
                Some(author) => {
                    format!("{committer} committed {commit_link} (authored by {author}).")
                }
                None => format!("{committer} committed {commit_link}."),
            };
            Entry {
                title_html,
                date: time::format_utc(*date),
            }
        })
        .collect())
}

/// A `DiffusionPathChange` of the commit.
struct PathChange {
    path_id: i64,
    path: String,
    change_type: i64,
    file_type: i64,
}

/// `DiffusionPathChangeQuery` with a limit: the direct changes, by path,
/// and whether there were more.
async fn path_changes<S: WebState>(
    state: &S,
    commit: &Commit,
    limit: usize,
) -> Result<(Vec<PathChange>, bool), PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT c.pathID, c.changeType, c.fileType, p.path pathName FROM {} c \
         LEFT JOIN {} p ON c.pathID = p.id WHERE c.commitID = ? AND isDirect = 1 LIMIT ?",
        database.table("repository", "repository_pathchange"),
        database.table("repository", "repository_path"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(commit.id)
        .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let limited = found.len() > limit;
    let mut changes = Vec::new();
    for row in found.iter().take(limit) {
        let change_type = rows::int(row, "changeType")?;
        // TYPE_CHILD.
        if change_type == 10 {
            continue;
        }
        changes.push(PathChange {
            path_id: rows::int(row, "pathID")?,
            path: rows::opt_text(row, "pathName")?
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_owned(),
            change_type,
            file_type: rows::int(row, "fileType")?,
        });
    }
    // isort($raw_changes, 'pathName').
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((changes, limited))
}

/// The label of a change type in the table of contents.
fn change_label(change_type: i64) -> &'static str {
    match change_type {
        1 => "Added",
        3 => "Deleted",
        4 => "Moved Away",
        5 => "Copied Away",
        6 => "Moved Here",
        7 => "Copied Here",
        8 => "Deleted (Multiple Copies)",
        _ => "Modified",
    }
}

/// The changesets of an arcanist parse, as `DifferentialChangeset` rows.
fn to_changeset(path_id: i64, new: phab_diff::build::NewChangeset) -> Changeset {
    Changeset {
        id: path_id,
        phid: None,
        diff_id: 0,
        old_file: new.old_file,
        filename: new.filename,
        away_paths: new
            .away_paths
            .as_array()
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        change_type: ChangeType::from_value(new.change_type),
        file_type: FileType::from_value(new.file_type),
        metadata: new.metadata.map_or(Value::Null, Value::Object),
        old_properties: new.old_properties,
        new_properties: new.new_properties,
        add_lines: new.add_lines,
        del_lines: new.del_lines,
        hunks: new.hunks,
    }
}

/// `diffusion.diffquery` for one path: the changeset of `git diff` with
/// full context against `against` (the commit's first parent), with the
/// path's ID.
pub(crate) async fn changeset(
    context: &Context,
    commit: &str,
    against: &str,
    path: &str,
    path_id: i64,
) -> Result<Option<Changeset>, PageError> {
    let path = if path.is_empty() { "." } else { path };
    let raw = context
        .drequest
        .copy
        .execx(
            [
                "diff",
                "-M",
                "-C",
                "--no-ext-diff",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "-U65535",
                against,
                commit,
                "--",
                path,
            ]
            .map(str::to_owned)
            .to_vec(),
            None,
        )
        .await
        .map_err(method_error)?;
    if raw.is_empty() {
        return Ok(None);
    }
    let raw =
        String::from_utf8(raw).map_err(|_| PageError::Fallback("a diff which isn't UTF-8"))?;
    let changes = match phab_diff::arcanist::parse_diff(&raw) {
        Ok(changes) => changes,
        Err(phab_diff::arcanist::ParseError::Empty) => return Ok(None),
        Err(phab_diff::arcanist::ParseError::Unsupported(reason)) => {
            return Err(PageError::Fallback(reason));
        }
    };
    let (changesets, _) = phab_diff::build::changesets(&changes)
        .map_err(|phab_diff::build::Unsupported(reason)| PageError::Fallback(reason))?;
    Ok(changesets
        .into_iter()
        .next()
        .map(|new| to_changeset(path_id, new)))
}

/// The changesets as the revision page renders them.
pub(crate) async fn render_changesets<S: WebState>(
    state: &S,
    context: &Context,
    built: Vec<Changeset>,
) -> Result<Vec<String>, PageError> {
    let searchfox = state.searchfox_trees().tree_for(&context.repository);
    let options = phab_diff::view::ViewOptions {
        diff_command: crate::revision::diff_command(),
        ..phab_diff::view::ViewOptions::default()
    };
    tokio::task::spawn_blocking(move || {
        built
            .iter()
            .map(|changeset| match phab_diff::view::build(changeset, &options) {
                Ok(view) => diff_html::changeset_with_inlines(&view, &[], searchfox.as_ref()),
                Err(error) => {
                    tracing::warn!(%error, path = changeset.filename, "changeset failed to render");
                    "<section class=\"changeset\"><p class=\"changeset-shield\">This file could \
                     not be shown.</p></section>"
                        .to_owned()
                }
            })
            .collect()
    })
    .await
    .map_err(harbormaster::internal)
}

/// The rendered changesets of the commit's changes.
async fn changesets<S: WebState>(
    state: &S,
    context: &Context,
    commit: &Commit,
    parents: &[String],
    changes: &[PathChange],
) -> Result<Vec<String>, PageError> {
    let against = match parents.first() {
        Some(_) => format!("{}^", commit.identifier),
        None => EMPTY_TREE.to_owned(),
    };
    let mut built = Vec::new();
    for change in changes {
        // Git has no directory changes.
        if change.file_type == 4 {
            continue;
        }
        if let Some(changeset) = changeset(
            context,
            &commit.identifier,
            &against,
            &change.path,
            change.path_id,
        )
        .await?
        {
            if matches!(changeset.file_type, FileType::Binary | FileType::Image) {
                return Err(PageError::Fallback("a commit with binary or image changes"));
            }
            built.push(changeset);
        }
    }
    render_changesets(state, context, built).await
}

/// A call whose failure PHP reports on the page (`loadCommitState()`):
/// Rust leaves those pages to PHP.
fn state_error(error: PageError) -> PageError {
    match error {
        PageError::Internal(_) => PageError::Fallback("a commit Git can't read"),
        other => other,
    }
}

/// The commit page of `hash` in the context's repository.
#[allow(clippy::too_many_lines)]
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    hash: String,
) -> Result<Response, PageError> {
    if state.config().nonempty_string("bugtraq.url").is_some() {
        return Err(PageError::Fallback("Bugtraq links"));
    }
    let repository = &context.repository;
    let mut found =
        commits::load(state.database(), repository.id, Identifiers::Prefix(&hash)).await?;
    let commit = match found.len() {
        0 => return Err(PageError::Fallback("a commit not imported yet, or unknown")),
        1 => found.remove(0),
        _ => return Err(PageError::Fallback("an ambiguous commit")),
    };
    if commit.audit_status != "none" || has_audit_requests(state, &commit).await? {
        return Err(PageError::Fallback("a commit with audits"));
    }
    if commit.has_flags(commits::IMPORTED_UNREACHABLE) {
        return Err(PageError::Fallback("an unreachable commit"));
    }
    if !commit.has_flags(commits::IMPORTED_PERMANENT) {
        return Err(PageError::Fallback("an unpublished commit"));
    }
    let timeline = timeline(state, &context, &commit).await?;

    // loadCommitState().
    let identifier = commit.identifier.clone();
    let parents = context
        .call(&Query(CommitParentsQuery), json!({"commit": identifier}))
        .await
        .map_err(state_error)?;
    let parents: Vec<String> = parents
        .as_array()
        .map(|parents| {
            parents
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let merge_changes = context
        .call(
            &Query(MergedCommitsQuery),
            json!({"commit": identifier, "limit": MERGE_LIMIT + 1}),
        )
        .await
        .map_err(state_error)?;
    let refs = context
        .call(&Query(RefsQuery), json!({"commit": identifier}))
        .await
        .map_err(state_error)?;

    let (changes, limited) = if commit.has_flags(commits::IMPORTED_ALL) {
        path_changes(state, &commit, 1000).await?
    } else {
        (Vec::new(), false)
    };
    let mut info = None;
    let mut toc = Vec::new();
    let mut rendered = Vec::new();
    if !commit.has_flags(commits::IMPORTED_ALL) {
        info = Some((
            "Still Importing...",
            "This commit is still importing. Changes will be visible once the import finishes.",
        ));
    } else if changes.is_empty() {
        info = Some((
            "Empty Commit",
            "This commit is empty and does not affect any paths.",
        ));
    } else if limited {
        info = Some((
            "Very Large Commit",
            "This commit is very large, and affects more than 1000 files. Changes are not shown.",
        ));
    } else if changes.len() > CHANGES_LIMIT {
        return Err(PageError::Fallback("a commit of more than 100 files"));
    } else {
        rendered = changesets(state, &context, &commit, &parents, &changes).await?;
        toc = changes
            .iter()
            .filter(|change| change.file_type != 4)
            .map(|change| Toc {
                path: change.path.clone(),
                change: change_label(change.change_type),
                history_href: context.uri(
                    "history",
                    Link {
                        path: Some(&change.path),
                        commit: Some(&identifier),
                        ..Link::default()
                    },
                ),
                browse_href: context.uri(
                    "browse",
                    Link {
                        path: Some(&change.path),
                        commit: Some(&identifier),
                        ..Link::default()
                    },
                ),
            })
            .collect();
    }

    // buildPropertyListView().
    let edges = edges::load_edges(
        state.database(),
        std::slice::from_ref(&commit.phid),
        &[
            edges::COMMIT_HAS_REVISION,
            COMMIT_REVERTS_COMMIT,
            COMMIT_REVERTED_BY_COMMIT,
        ],
        None,
    )
    .await?;
    let of_type = |edge_type: i64| -> Vec<Phid> {
        edges
            .iter()
            .filter(|edge| edge.edge_type == edge_type)
            .map(|edge| edge.dst.clone())
            .collect()
    };
    let revision = of_type(edges::COMMIT_HAS_REVISION).into_iter().next();
    let reverts = of_type(COMMIT_REVERTS_COMMIT);
    let reverted_by = of_type(COMMIT_REVERTED_BY_COMMIT);
    let parent_commits = commits::load(
        state.database(),
        repository.id,
        Identifiers::Exact(&parents),
    )
    .await?;
    let mut phids: Vec<Phid> = commit.user_phids();
    phids.extend(revision.clone());
    phids.extend(reverts.iter().cloned());
    phids.extend(reverted_by.iter().cloned());
    phids.extend(parent_commits.iter().map(|parent| parent.phid.clone()));
    let handles = harbormaster::load_handles(state, &context.scope, &phids).await?;
    let handle_list = |phids: &[Phid]| -> String {
        phids
            .iter()
            .filter_map(|phid| handles.get(phid))
            .map(Handle::link_html)
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut provenance = Vec::new();
    if let Some(author) = commit.author() {
        let date = commit
            .author_epoch()
            .map(time::format_utc)
            .unwrap_or_default();
        provenance.push((
            commit.person_html(&author, &handles),
            format!("Authored on {date}"),
        ));
    }
    if !commit.is_author_committer()
        && let Some(committer) = commit.committer()
    {
        provenance.push((
            commit.person_html(&committer, &handles),
            format!("Committed on {}", time::format_utc(commit.epoch)),
        ));
    }
    let mut properties = Vec::new();
    if let Some(revision) = &revision {
        properties.push((
            "Differential Revision",
            handle_list(std::slice::from_ref(revision)),
        ));
    }
    let parent_phids: Vec<Phid> = parents
        .iter()
        .filter_map(|hash| {
            parent_commits
                .iter()
                .find(|parent| parent.identifier == *hash)
                .map(|parent| parent.phid.clone())
        })
        .collect();
    if !parent_phids.is_empty() {
        properties.push(("Parents", handle_list(&parent_phids)));
    }
    let root = format!("{}commit/{identifier}", repository.uri());
    let refs_html: Vec<String> = refs
        .as_array()
        .map(|refs| {
            refs.iter()
                .filter_map(|reference| reference.get("ref").and_then(Value::as_str))
                .map(|name| {
                    let href = context.uri(
                        "browse",
                        Link {
                            branch: Some(name),
                            commit: Some(&hash),
                            path: Some(""),
                            ..Link::default()
                        },
                    );
                    format!(
                        "<a href=\"{}\">{}</a>",
                        diff_html::escape(&href),
                        diff_html::escape(name)
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !refs_html.is_empty() {
        properties.push(("References", refs_html.join(", ")));
    }
    if !reverts.is_empty() {
        properties.push(("Reverts", handle_list(&reverts)));
    }
    if !reverted_by.is_empty() {
        properties.push(("Reverted By", handle_list(&reverted_by)));
    }

    // buildMergesTable().
    let mut merged: Vec<(String, Option<i64>)> = merge_changes
        .as_array()
        .map(|merges| {
            merges
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
    let merges_limited = merged.len() > MERGE_LIMIT;
    merged.truncate(MERGE_LIMIT);
    let merge_items = history::items(state, viewer, &context, &merged).await?;

    let message = commit.message().unwrap_or_default().to_owned();
    let markup = remarkup::render(state, &viewer.user.viewer(), &message, Mode::Html).await?;
    let title = if commit.summary.is_empty() {
        "Commit Detail".to_owned()
    } else {
        commit.summary.clone()
    };
    let display_name = repository.format_commit_name(&identifier);
    Ok(Page(CommitPage {
        shell: context.shell(state, viewer, &display_name),
        title,
        name: local_name(repository, &identifier),
        commit_href: repository.commit_uri(&identifier),
        edit_href: format!("/diffusion/commit/edit/{}/", commit.id),
        raw_diff_href: format!("{}?diff=1", repository.commit_uri(&identifier)),
        message_html: String::from_utf8_lossy(markup.as_bytes()).into_owned(),
        provenance,
        properties,
        branches_uri: Some(format!("{root}/branches/")),
        tags_uri: Some(format!("{root}/tags/")),
        timeline,
        merges: merge_items,
        merges_limited,
        info,
        toc,
        changesets: rendered,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commit_monograms() {
        assert_eq!(
            parse_monogram("rGITabc123"),
            Some(("GIT".to_owned(), "abc123".to_owned()))
        );
        assert_eq!(
            parse_monogram("R2:abc"),
            Some(("2".to_owned(), "abc".to_owned()))
        );
        assert_eq!(parse_monogram("D12"), None);
        assert_eq!(parse_monogram("rGIT"), None);
        assert_eq!(parse_monogram("R0:abc"), None);
        assert_eq!(parse_monogram("R2:xyz"), None);
        assert_eq!(parse_monogram("rgit1"), None);
    }

    #[test]
    fn labels_changes() {
        assert_eq!(change_label(1), "Added");
        assert_eq!(change_label(2), "Modified");
    }
}
