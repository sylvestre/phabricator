//! `/source/<x>/browse/<branch>/<path>` (`DiffusionBrowseController`): a
//! directory, with its README and the open revisions touching it, or a
//! file, highlighted when a grammar covers it.
//!
//! Left to PHP: searches (`?grep=`), the other views of a file (raw,
//! blame, `?before=`, document engines), binary, image, notebook and
//! very large files, Git LFS, and directories of a branch with lint.

use std::fmt::Write as _;

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::browse_query::BrowseQuery;
use phab_app_diffusion::history;
use phab_app_diffusion::tags_query::TagsQuery;
use phab_remarkup::Mode;
use serde_json::{Value, json};

use super::{Context, Link, Pager, Stable, Tab, Tag, local_name};
use crate::error::PageError;
use crate::harbormaster::{internal, method_error};
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, highlight, remarkup};

/// Files larger than this go to PHP, which stores them as files first.
const BYTE_LIMIT: u64 = 1024 * 1024;

/// `DifferentialChangeType::FILE_DIRECTORY`.
const FILE_DIRECTORY: i64 = 4;
/// `DifferentialChangeType::FILE_SUBMODULE`.
const FILE_SUBMODULE: i64 = 8;

/// A row of `DiffusionBrowseTableView`.
pub(crate) struct Entry {
    /// The name, with a slash for directories.
    pub(crate) name: String,
    pub(crate) href: String,
    pub(crate) icon: &'static str,
    pub(crate) strong: bool,
    pub(crate) history_href: String,
    /// The key of the row in `diffusion.lastmodifiedquery`'s answer.
    pub(crate) full_path: String,
}

/// A directory listing, with the last change of each entry filled in by
/// PHP's `lastmodified` endpoint, as PHP's page does.
pub(crate) struct Table {
    pub(crate) entries: Vec<Entry>,
    pub(crate) lastmodified_uri: String,
}

/// `DiffusionReadmeView`.
pub(crate) struct Readme {
    pub(crate) name: String,
    pub(crate) html: String,
    /// `remarkup-code` for text.
    pub(crate) class: &'static str,
}

/// An open revision touching the path.
pub(crate) struct OpenRevision {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) status: &'static str,
}

/// What a page above the listing shows: `PHUIHeaderView`.
pub(crate) struct Header {
    pub(crate) title_html: String,
    pub(crate) icon: &'static str,
    pub(crate) tags: Vec<Tag>,
    pub(crate) tabs: Vec<Tab>,
    /// Where the pattern search form goes.
    pub(crate) search_uri: Option<String>,
}

impl Header {
    pub(crate) fn new(title_html: String, icon: &'static str) -> Self {
        Self {
            title_html,
            icon,
            tags: Vec::new(),
            tabs: Vec::new(),
            search_uri: None,
        }
    }

    pub(crate) fn tag(mut self, tag: Tag) -> Self {
        self.tags.push(tag);
        self
    }

    pub(crate) fn tabs(mut self, tabs: Vec<Tab>) -> Self {
        self.tabs = tabs;
        self
    }

    pub(crate) fn search(mut self, uri: String) -> Self {
        self.search_uri = Some(uri);
        self
    }
}

/// A button of the action bar.
pub(crate) struct Button {
    pub(crate) text: &'static str,
    pub(crate) icon: &'static str,
    pub(crate) href: String,
}

#[derive(Template)]
#[template(path = "diffusion_directory.html")]
pub(crate) struct DirectoryPage {
    shell: crate::Shell,
    header: Header,
    buttons: Vec<Button>,
    tag: Option<(String, Option<String>)>,
    empty: Option<(&'static str, String)>,
    title: String,
    table: Option<Table>,
    pager: Pager,
    open_revisions: Vec<OpenRevision>,
    readme: Option<Readme>,
}

/// A line of a file.
pub(crate) struct Line {
    number: usize,
    href: String,
    html: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "diffusion_file.html")]
pub(crate) struct FilePage {
    shell: crate::Shell,
    header: Header,
    buttons: Vec<Button>,
    name: String,
    lines: Vec<Line>,
    open_revisions: Vec<OpenRevision>,
}

/// `getIconForFileType()`.
fn file_icon(file_type: i64) -> &'static str {
    match file_type {
        1 | 7 => "fa-file-text-o",
        2 => "fa-file-image-o",
        4 => "fa-folder",
        5 => "fa-link",
        8 => "fa-folder-open-o",
        _ => "fa-file",
    }
}

/// `DiffusionBrowseTableView` of the `paths` of `diffusion.browsequery`.
pub(crate) fn table(context: &Context, stable: &Stable, paths: &[Value]) -> Table {
    let base = context.path.trim_matches('/');
    let base = if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    };
    let mut entries = Vec::new();
    for path in paths {
        let text = |key: &str| path.get(key).and_then(Value::as_str).unwrap_or_default();
        let file_type = path.get("fileType").and_then(Value::as_i64).unwrap_or(0);
        let full_path = format!("{base}{}", text("path"));
        let browse = |path: &str| {
            context.uri(
                "browse",
                Link {
                    path: Some(path),
                    ..Link::default()
                },
            )
        };
        let (name, href, strong, history_path, key) = match file_type {
            FILE_DIRECTORY => (
                format!("{}/", text("path")),
                browse(&format!("{full_path}/")),
                true,
                format!("{full_path}/"),
                format!("{full_path}/"),
            ),
            FILE_SUBMODULE => {
                let mut href = String::from("/diffusion/external/");
                let mut query = form_urlencoded::Serializer::new(String::new());
                query.append_pair("uri", text("externalURI"));
                query.append_pair("id", text("hash"));
                href.push('?');
                href.push_str(&query.finish());
                (
                    format!("{}/", text("path")),
                    href,
                    true,
                    format!("{full_path}/"),
                    full_path.clone(),
                )
            }
            _ => (
                text("path").to_owned(),
                browse(&full_path),
                false,
                full_path.clone(),
                full_path.clone(),
            ),
        };
        entries.push(Entry {
            name,
            href,
            icon: file_icon(file_type),
            strong,
            history_href: context.uri(
                "history",
                Link {
                    path: Some(&history_path),
                    ..Link::default()
                },
            ),
            full_path: key,
        });
    }
    Table {
        entries,
        lastmodified_uri: context.uri(
            "lastmodified",
            Link {
                commit: Some(&stable.hash),
                ..Link::default()
            },
        ),
    }
}

/// `getReadmePriority()`.
fn readme_priority(name: &str) -> Option<i32> {
    let lower = name.to_lowercase();
    if !(lower == "readme" || lower.starts_with("readme.")) {
        return None;
    }
    if lower == "readme" {
        return Some(90);
    }
    Some(match lower.rsplit('.').next().unwrap_or_default() {
        "remarkup" => 100,
        "rainbow" => 80,
        "md" => 70,
        "txt" => 60,
        _ => 50,
    })
}

/// The content of `path` at `commit`, or `None` when it is too large.
async fn file_content(
    context: &Context,
    commit: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, PageError> {
    let object = format!("{commit}:{path}");
    let copy = &context.drequest.copy;
    let size = copy
        .execx(
            vec!["cat-file".to_owned(), "-s".to_owned(), object.clone()],
            None,
        )
        .await
        .map_err(method_error)?;
    let size: u64 = String::from_utf8_lossy(&size)
        .trim()
        .parse()
        .map_err(|_| PageError::Fallback("an unusual object size"))?;
    if size > BYTE_LIMIT {
        return Ok(None);
    }
    // DiffusionGitFileContentQuery.
    copy.execx(
        vec![
            "cat-file".to_owned(),
            "blob".to_owned(),
            "--".to_owned(),
            object,
        ],
        None,
    )
    .await
    .map(Some)
    .map_err(method_error)
}

/// `renderDirectoryReadme()`: the preferred README of the listing.
pub(crate) async fn readme<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
    stable: &Stable,
    paths: &[Value],
) -> Result<Option<Readme>, PageError> {
    let mut best: Option<(i32, &str, &str)> = None;
    for path in paths {
        let file_type = path.get("fileType").and_then(Value::as_i64);
        if !matches!(file_type, Some(1 | 7)) {
            continue;
        }
        let local = path.get("path").and_then(Value::as_str).unwrap_or_default();
        let full = path
            .get("fullPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(priority) = readme_priority(local) {
            // arsort() keeps the first of equal priorities.
            if best.is_none_or(|(known, _, _)| priority > known) {
                best = Some((priority, full, local));
            }
        }
    }
    let Some((_, full, local)) = best else {
        return Ok(None);
    };
    // PHP shows no README when loading it fails.
    let Ok(Some(data)) = file_content(context, &stable.hash, full).await else {
        return Ok(None);
    };
    if data.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    let lower = local.to_lowercase();
    let extension = lower.rsplit('.').next().unwrap_or_default();
    let is_remarkup = lower == "readme" || matches!(extension, "remarkup" | "md");
    let (html, class) = if is_remarkup {
        let markup = remarkup::render(state, &viewer.user.viewer(), &text, Mode::Html).await?;
        (
            String::from_utf8_lossy(markup.as_bytes()).into_owned(),
            "remarkup",
        )
    } else {
        (
            diff_html::escape(&text).replace('\n', "<br />\n"),
            if extension == "rainbow" {
                "remarkup-code"
            } else {
                "readme-text"
            },
        )
    };
    Ok(Some(Readme {
        name: local.rsplit('/').next().unwrap_or(local).to_owned(),
        html,
        class,
    }))
}

/// `buildOpenRevisions()`: open revisions of the repository touching the
/// path, updated in the last 30 days.
pub(crate) async fn open_revisions<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
) -> Result<Vec<OpenRevision>, PageError> {
    let database = state.database();
    let normal = history::normalize_path(&context.path);
    let Some(path_id) = history::path_id(database, &normal)
        .await
        .map_err(method_error)?
    else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT DISTINCT r.id, r.dateModified FROM {} r JOIN {} paths ON paths.revisionID = r.id \
         WHERE paths.pathID = ? AND paths.repositoryID = ? AND r.dateModified >= ? \
         AND r.status NOT IN ('published', 'abandoned') ORDER BY r.dateModified DESC LIMIT 100",
        database.table("differential", "differential_revision"),
        database.table("differential", "differential_affectedpath"),
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(path_id)
        .bind(context.repository.id)
        .bind(state.now() - 30 * 86_400)
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|row| phab_db::rows::int(row, "id"))
        .collect::<Result<_, _>>()?;
    let revisions = phab_app_differential::storage::load_revisions_by_id(database, &ids).await?;
    let mut revisions = phab_app_differential::storage::visible_revisions(
        database,
        state.config(),
        &viewer.user.viewer(),
        revisions,
        state.now(),
    )
    .await
    .map_err(internal)?;
    revisions.sort_by_key(|revision| std::cmp::Reverse((revision.date_modified, revision.id)));
    revisions.truncate(10);
    Ok(revisions
        .into_iter()
        .map(|revision| OpenRevision {
            id: revision.id,
            status: crate::revision::status_name(&revision.status),
            title: revision.title,
        })
        .collect())
}

/// Whether the page's branch has lint messages, which PHP's table shows
/// in a column of its own.
async fn has_lint<S: WebState>(state: &S, context: &Context) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT lintCommit FROM {} WHERE repositoryID = ? AND name = ?",
        database.table("repository", "repository_branch")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(context.repository.id)
        .bind(&context.branch)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    Ok(match row {
        Some(row) => {
            phab_db::rows::opt_text(&row, "lintCommit")?.is_some_and(|commit| !commit.is_empty())
        }
        None => false,
    })
}

/// The header of browse pages (`buildHeaderView()`).
fn header(context: &Context, stable: &Stable, icon: &'static str) -> Header {
    let path = if context.path.is_empty() {
        "/"
    } else {
        context.path.as_str()
    };
    Header::new(context.path_links_html("browse"), icon)
        .search(context.uri(
            "browse",
            Link {
                path: Some(path),
                ..Link::default()
            },
        ))
        .tag(context.commit_tag(stable))
        .tag(context.branch_tag())
        .tabs(context.tabs("code"))
}

/// `buildActionButtons()`: Compare, Back to HEAD and History.
fn action_buttons(context: &Context, is_directory: bool) -> Vec<Button> {
    let mut buttons = Vec::new();
    if is_directory {
        buttons.push(Button {
            text: "Compare",
            icon: "fa-code-fork",
            href: context.uri("compare", Link::default()),
        });
    }
    if context
        .commit
        .as_deref()
        .is_some_and(|commit| !commit.is_empty())
    {
        buttons.push(Button {
            text: "Back to HEAD",
            icon: "fa-home",
            href: context.uri(
                "browse",
                Link {
                    commit: Some(""),
                    ..Link::default()
                },
            ),
        });
    }
    buttons.push(Button {
        text: "History",
        icon: "fa-history",
        href: context.uri("history", Link::default()),
    });
    buttons
}

/// `DiffusionEmptyResultView`: why there is nothing to list.
fn empty_result(context: &Context, result: &Value) -> Result<(&'static str, String), PageError> {
    let repository = &context.repository;
    let commit = match context
        .commit
        .as_deref()
        .filter(|commit| !commit.is_empty())
    {
        Some(commit) => repository.format_commit_name(commit),
        None => "HEAD".to_owned(),
    };
    let commit = diff_html::escape(&commit);
    let reason = result
        .get("reasonForEmptyResultSet")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(match reason {
        "nonexistent" => (
            "Path Does Not Exist",
            "This path does not exist anywhere.".to_owned(),
        ),
        "empty" => (
            "Empty Directory",
            format!("This path was an empty directory at {commit}."),
        ),
        "is-submodule" => (
            "Submodule",
            format!("This path was a submodule at {commit}."),
        ),
        "is-deleted" => {
            let hash = |key: &str| {
                result
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let deleted = hash("deletedAtCommit");
            let existed = hash("existedAtCommit");
            let existed_href = context.uri(
                "browse",
                Link {
                    commit: Some(&existed),
                    ..Link::default()
                },
            );
            (
                "Path Was Deleted",
                format!(
                    "This path does not exist at {commit}. It was deleted in <a href=\"{}\">{}</a> \
                     and last existed at <a href=\"{}\">{}</a>.",
                    diff_html::escape(&repository.commit_uri(&deleted)),
                    diff_html::escape(&local_name(repository, &deleted)),
                    diff_html::escape(&existed_href),
                    diff_html::escape(&repository.format_commit_name(&existed)),
                ),
            )
        }
        _ => return Err(PageError::Fallback("an unusual empty listing")),
    })
}

/// `buildPropertyView()`: the tag the page is at, and its message.
async fn tag_property<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
    stable: &Stable,
) -> Result<Option<(String, Option<String>)>, PageError> {
    if stable.kind != "tag" {
        return Ok(None);
    }
    let Some(symbolic) = context.commit.clone() else {
        return Ok(None);
    };
    let tags = context
        .call(
            &phab_app_diffusion::query::Query(TagsQuery),
            json!({"names": [symbolic], "needMessages": true}),
        )
        .await?;
    let message = tags
        .as_array()
        .and_then(|tags| {
            tags.iter()
                .rev()
                .find(|tag| tag.get("name").and_then(Value::as_str) == Some(symbolic.as_str()))
        })
        .and_then(|tag| tag.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map(str::to_owned);
    let message = match message {
        Some(message) => {
            let markup =
                remarkup::render(state, &viewer.user.viewer(), &message, Mode::Html).await?;
            Some(String::from_utf8_lossy(markup.as_bytes()).into_owned())
        }
        None => None,
    };
    Ok(Some((symbolic, message)))
}

/// `GET /source/<x>/browse/<blob>`.
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    offset: i64,
) -> Result<Response, PageError> {
    let stable = context.stable().await?;
    let result = context
        .call(
            &phab_app_diffusion::query::Query(BrowseQuery),
            json!({
                "path": context.path,
                "commit": stable.hash,
                "offset": offset,
                "limit": super::PAGE_SIZE + 1,
            }),
        )
        .await?;
    if result
        .get("reasonForEmptyResultSet")
        .and_then(Value::as_str)
        == Some("is-file")
    {
        return Ok(file(state, viewer, &context, &stable)
            .await?
            .into_response());
    }
    if has_lint(state, &context).await? {
        return Err(PageError::Fallback("a branch with lint messages"));
    }
    let valid = result
        .get("isValidResults")
        .is_some_and(phab_core::php::is_truthy);
    let mut paths: Vec<Value> = result
        .get("paths")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let base = context.uri("browse", Link::default());
    let pager = Pager::slice(offset, &base, &mut paths);
    let (empty, table) = if valid {
        (None, Some(table(&context, &stable, &paths)))
    } else {
        (Some(empty_result(&context, &result)?), None)
    };
    let name = context
        .path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("/")
        .to_owned();
    let readme = readme(state, viewer, &context, &stable, &paths).await?;
    Ok(Page(DirectoryPage {
        shell: context.shell(state, viewer, &context.title(&name)),
        header: header(&context, &stable, "fa-folder-open"),
        buttons: action_buttons(&context, true),
        tag: tag_property(state, viewer, &context, &stable).await?,
        empty,
        title: name,
        table,
        pager,
        open_revisions: open_revisions(state, viewer, &context).await?,
        readme,
    })
    .into_response())
}

/// Files PHP renders with a document engine other than source code.
fn is_document(path: &str) -> bool {
    let lower = path.to_lowercase();
    let extension = lower.rsplit('.').next().unwrap_or_default();
    matches!(
        extension,
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "webp" | "bmp" | "ipynb" | "pdf"
    )
}

/// `browseFile()`: the file as source code.
async fn file<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
    stable: &Stable,
) -> Result<Page<FilePage>, PageError> {
    if is_document(&context.path) {
        return Err(PageError::Fallback("an image, notebook or PDF file"));
    }
    let commit = context.commit().await?;
    let data = file_content(context, &commit, &context.path)
        .await?
        .ok_or(PageError::Fallback("a very large file"))?;
    if data.contains(&0) {
        return Err(PageError::Fallback("a binary file"));
    }
    if data.starts_with(b"version https://git-lfs.github.com/spec/v1") {
        return Err(PageError::Fallback("a Git LFS pointer"));
    }
    let text =
        String::from_utf8(data).map_err(|_| PageError::Fallback("a file which isn't UTF-8"))?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let tokens = highlight::syntax_for(&context.path)
        .and_then(|syntax| highlight::highlight_lines(syntax, &lines));
    let selected = selected_lines(context.line.as_deref());
    let line_base = context.uri(
        "browse",
        Link {
            commit: Some(&commit),
            ..Link::default()
        },
    );
    let lines = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let body = body.strip_suffix('\r').unwrap_or(body);
            let html = match tokens.as_ref().and_then(|tokens| tokens.get(index)) {
                Some(tokens) => highlighted(body, tokens),
                None => diff_html::escape(body),
            };
            let number = index + 1;
            Line {
                number,
                href: format!("{line_base}${number}"),
                html,
                selected: selected.iter().any(|range| range.contains(&number)),
            }
        })
        .collect();
    let name = context
        .path
        .rsplit('/')
        .next()
        .unwrap_or(&context.path)
        .to_owned();
    let mut buttons = vec![Button {
        text: "Last Change",
        icon: "fa-backward",
        href: context.uri("change", Link::default()),
    }];
    buttons.extend(action_buttons(context, false));
    let mut raw = context.uri("browse", Link::default());
    raw.push_str("?view=raw");
    buttons.push(Button {
        text: "Raw File",
        icon: "fa-file-text",
        href: raw,
    });
    Ok(Page(FilePage {
        shell: context.shell(state, viewer, &context.title(&name)),
        header: header(context, stable, "fa-file-code-o"),
        buttons,
        name,
        lines,
        open_revisions: open_revisions(state, viewer, context).await?,
    }))
}

/// The HTML of a line with its syntax tokens.
fn highlighted(line: &str, tokens: &[highlight::Token]) -> String {
    let mut html = String::with_capacity(line.len() * 2);
    let mut start = 0;
    for token in tokens {
        let end = token.range.end.min(line.len());
        if token.range.start < start || token.range.start >= end {
            continue;
        }
        html.push_str(&diff_html::escape(&line[start..token.range.start]));
        let _ = write!(
            html,
            "<span class=\"{}\">{}</span>",
            token.class,
            diff_html::escape(&line[token.range.start..end])
        );
        start = end;
    }
    html.push_str(&diff_html::escape(&line[start..]));
    html
}

/// The lines of a `$12`, `$12-15` or `$1,4-5` range.
fn selected_lines(line: Option<&str>) -> Vec<std::ops::RangeInclusive<usize>> {
    let Some(line) = line else {
        return Vec::new();
    };
    line.split(',')
        .filter_map(|part| {
            let (first, last) = part.split_once('-').unwrap_or((part, part));
            let first: usize = first.parse().ok()?;
            let last: usize = last.parse().ok()?;
            Some(first.min(last)..=first.max(last))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_readmes_like_php() {
        assert_eq!(readme_priority("README"), Some(90));
        assert_eq!(readme_priority("readme.md"), Some(70));
        assert_eq!(readme_priority("README.remarkup"), Some(100));
        assert_eq!(readme_priority("README.rst"), Some(50));
        assert_eq!(readme_priority("READMEFIRST"), None);
    }

    #[test]
    fn reads_line_ranges() {
        assert_eq!(selected_lines(Some("3")), vec![3..=3]);
        assert_eq!(selected_lines(Some("5-2,9")), vec![2..=5, 9..=9]);
        assert_eq!(selected_lines(None), Vec::new());
    }
}
