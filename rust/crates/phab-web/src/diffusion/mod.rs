//! Diffusion pages of Git repositories (W13), read-only: the repository
//! home (`/source/<short name>/`, `/diffusion/<callsign or ID>/`), browse
//! (directories and files), history, branches, tags and commits
//! (`/rXYZ<hash>`, `/R<id>:<hash>`).
//!
//! The pages call the `diffusion.*` query methods as the viewer, as PHP's
//! controllers do through `callConduitWithDiffusionRequest()`, and fall
//! back to PHP for what they don't render (`421`): Mercurial and
//! Subversion repositories, hosted and cluster repositories, a working
//! copy this host doesn't have, blame and other views, very large, binary
//! and image files, commits with audits or inline comments, searches,
//! and query strings other than the pager's.

mod browse;
mod commit;
mod commits;
mod history;
mod home;
mod refs;
mod uri;

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_app_differential::repository::{self, Repository};
use phab_app_diffusion::drequest::DiffusionRequest;
use phab_app_diffusion::refs::BRANCH;
use phab_conduit::Method;
use serde_json::{Map, Value, json};

pub(crate) use commit::{parse_monogram as parse_commit_monogram, show as commit};

use crate::error::PageError;
use crate::harbormaster::{Scope, method_error};
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, harbormaster};
use uri::{Blob, Target};

/// `PhabricatorDiffusionApplication`.
const APPLICATION: &str = phab_app_diffusion::APPLICATION.class;

/// `PHUIPagerView`'s default page size.
const PAGE_SIZE: i64 = 100;

/// How the request named the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    /// `/source/<short name>/`.
    Source,
    /// `/diffusion/<callsign or ID>/`.
    Diffusion,
    /// A commit monogram, which names the repository by callsign or ID.
    Commit,
}

/// A page of a repository: the viewer, the repository and the branch,
/// path and commit of the URI (`DiffusionRequest`).
pub(crate) struct Context {
    scope: Scope,
    repository: Repository,
    drequest: DiffusionRequest,
    branch: String,
    path: String,
    /// The symbolic commit of the URI, if any.
    commit: Option<String>,
    /// The `$line` of the URI, if any.
    line: Option<String>,
    /// The request's path and query, for the shell.
    current: String,
}

/// `getStableCommit()` with `getSymbolicType()`.
pub(crate) struct Stable {
    hash: String,
    kind: String,
}

impl Context {
    /// `loadDiffusionContext()`: the repository the viewer asked for, or
    /// why there is no page.
    async fn load<S: WebState>(
        state: &S,
        viewer: &WebUser,
        prefix: Prefix,
        identifier: &str,
        blob: Option<Blob>,
        uri: &Uri,
    ) -> Result<Self, Box<Response>> {
        viewer
            .require_application(state, APPLICATION)
            .await
            .map_err(|error| Box::new(error.into_response()))?;
        let identifier = match prefix {
            Prefix::Source => identifier.strip_suffix(".git").unwrap_or(identifier),
            Prefix::Diffusion | Prefix::Commit => {
                let callsign = !identifier.is_empty()
                    && identifier.bytes().all(|byte| byte.is_ascii_uppercase());
                let id = identifier.starts_with(|first: char| ('1'..='9').contains(&first))
                    && identifier.bytes().all(|byte| byte.is_ascii_digit());
                if !callsign && !id {
                    return Err(Box::new(crate::fallback_response(
                        "a Diffusion page not ported",
                    )));
                }
                identifier
            }
        };
        let database = state.database();
        let found = repository::load_by_identifiers(database, &[identifier.to_owned()])
            .await
            .map_err(|error| Box::new(PageError::from(error).into_response()))?;
        let Some(found) = found.into_values().next() else {
            return Err(Box::new(PageError::NotFound.into_response()));
        };
        let repository = repository::visible(
            database,
            state.config(),
            &viewer.user.viewer(),
            vec![found],
            state.now(),
        )
        .await
        .map_err(|error| Box::new(harbormaster::internal(error).into_response()))?
        .pop()
        .ok_or_else(|| Box::new(PageError::PolicyDenied.into_response()))?;

        // getCanonicalPath(): the repository's own prefix.
        if prefix != Prefix::Commit {
            let segment = match prefix {
                Prefix::Source => "source",
                _ => "diffusion",
            };
            let raw = uri.path();
            let after_prefix = raw
                .strip_prefix(&format!("/{segment}/"))
                .and_then(|rest| rest.find('/').map(|slash| &rest[slash + 1..]));
            if let Some(suffix) = after_prefix {
                let canonical = format!("{}{suffix}", repository.uri());
                if canonical != raw {
                    return Err(Box::new(redirect(&canonical)));
                }
            }
        }

        if repository.vcs != "git" {
            return Err(Box::new(crate::fallback_response(
                "Mercurial and Subversion repositories",
            )));
        }
        if repository
            .detail("hosting-enabled")
            .is_some_and(phab_core::php::is_truthy)
        {
            return Err(Box::new(crate::fallback_response("a hosted repository")));
        }
        let blob = blob.unwrap_or_default();
        let scope = Scope::new(state, viewer);
        let mut params = Map::new();
        params.insert("repository".to_owned(), json!(repository.phid.as_str()));
        params.insert("branch".to_owned(), json!(blob.branch));
        params.insert("path".to_owned(), json!(blob.path));
        params.insert("commit".to_owned(), json!(blob.commit));
        let drequest = DiffusionRequest::from_query(&scope.request_with(params))
            .await
            .map_err(|error| Box::new(method_error(error).into_response()))?;
        let branch = drequest
            .branch()
            .map_err(|error| Box::new(method_error(error).into_response()))?;
        Ok(Self {
            scope,
            repository,
            drequest,
            branch,
            path: blob.path.unwrap_or_default(),
            commit: blob.commit,
            line: blob.line,
            current: uri
                .path_and_query()
                .map_or("/", |path| path.as_str())
                .to_owned(),
        })
    }

    /// `callConduitWithDiffusionRequest()`: `method` with `params`, and the
    /// repository and branch of the page where the method takes them.
    async fn call(&self, method: &dyn Method, params: Value) -> Result<Value, PageError> {
        let Value::Object(mut params) = params else {
            return Err(harbormaster::internal(std::io::Error::other(
                "parameters which are not a map",
            )));
        };
        let takes = method.params();
        if takes.contains(&"repository") {
            params
                .entry("repository")
                .or_insert_with(|| json!(self.repository.phid.as_str()));
        }
        if takes.contains(&"branch") {
            params.entry("branch").or_insert_with(|| json!(self.branch));
        }
        method
            .execute(&self.scope.request_with(params))
            .await
            .map_err(method_error)
    }

    /// `getStableCommit()`, and how the symbolic commit resolved
    /// (`getSymbolicType()`): `commit`, `branch` or `tag`.
    async fn stable(&self) -> Result<Stable, PageError> {
        if let Some(commit) = &self.commit
            && commit.len() == 40
            && commit
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'f' | b'0'..=b'9'))
        {
            return Ok(Stable {
                hash: commit.clone(),
                kind: "commit".to_owned(),
            });
        }
        // queryStableCommit().
        let (reference, types) = match self.commit.as_deref().filter(|commit| !commit.is_empty()) {
            Some(commit) => (commit.to_owned(), Vec::new()),
            None => (self.branch.clone(), vec![BRANCH.to_owned()]),
        };
        let request = self.scope.request();
        let mut resolved = self
            .drequest
            .resolve_refs(&request, std::slice::from_ref(&reference), &types)
            .await
            .map_err(method_error)?;
        let matches = resolved
            .shift_remove(&phab_app_diffusion::php::Key::from_text(&reference))
            .unwrap_or_default();
        let matches: Vec<&Value> = matches.iter().map(|(_, info)| info).collect();
        // chooseBestRefMatch(): the first open one.
        let best = matches
            .iter()
            .find(|info| {
                !(info.get("type").and_then(Value::as_str) == Some(BRANCH)
                    && info.get("closed").is_some_and(phab_core::php::is_truthy))
            })
            .or(matches.first())
            .ok_or(PageError::Fallback("a ref which doesn't exist"))?;
        Ok(Stable {
            hash: best
                .get("identifier")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            kind: best
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// `getCommit()`: the symbolic commit, or the stable one.
    async fn commit(&self) -> Result<String, PageError> {
        match &self.commit {
            Some(commit) => Ok(commit.clone()),
            None => Ok(self.stable().await?.hash),
        }
    }

    /// `DiffusionRequest::generateURI()`: `target` with the page's path,
    /// branch and symbolic commit where it has none.
    fn uri(&self, action: &str, target: Link<'_>) -> String {
        uri::generate(
            &self.repository,
            action,
            Target {
                path: target.path.unwrap_or(&self.path),
                branch: target.branch.unwrap_or(&self.branch),
                commit: target
                    .commit
                    .unwrap_or_else(|| self.commit.as_deref().unwrap_or("")),
                line: target.line.unwrap_or(""),
            },
        )
    }

    /// The default branch (`getDefaultBranch()`).
    fn default_branch(&self) -> String {
        self.repository.default_branch().unwrap_or_default()
    }

    /// `renderBranchTag()`.
    fn branch_tag(&self) -> Tag {
        Tag {
            text: truncate(&self.branch, 24),
            href: None,
            icon: None,
            outline: true,
        }
    }

    /// `renderCommitHashTag()`.
    fn commit_tag(&self, stable: &Stable) -> Tag {
        Tag {
            text: local_name(&self.repository, &stable.hash),
            href: Some(self.repository.commit_uri(&stable.hash)),
            icon: None,
            outline: false,
        }
    }

    /// `renderSymbolicCommit()`.
    fn symbolic_tag(&self) -> Option<Tag> {
        self.commit.as_ref().map(|commit| Tag {
            text: truncate(commit, 24),
            href: None,
            icon: Some("fa-tag"),
            outline: false,
        })
    }

    /// `buildTabsView()`.
    fn tabs(&self, selected: &str) -> Vec<Tab> {
        [
            ("code", "Code", "fa-code", "browse"),
            ("branch", "Branches", "fa-code-fork", "branches"),
            ("tags", "Tags", "fa-tags", "tags"),
            ("history", "History", "fa-history", "history"),
        ]
        .into_iter()
        .map(|(key, name, icon, action)| Tab {
            name,
            icon,
            href: self.uri(action, Link::default()),
            selected: key == selected,
        })
        .collect()
    }

    /// `renderPathLinks()`: the repository and each directory of the path,
    /// linked to `action`.
    fn path_links_html(&self, action: &str) -> String {
        let divider = "<span class=\"path-divider\">/</span>";
        let name = diff_html::escape(&self.repository.display_name());
        let parts: Vec<&str> = self
            .path
            .trim_matches('/')
            .split('/')
            .filter(|part| !part.is_empty() && *part != "0")
            .collect();
        if parts.is_empty() {
            return format!("{name}{divider}");
        }
        let root = self.uri(
            action,
            Link {
                path: Some(""),
                ..Link::default()
            },
        );
        let mut html = format!(
            "<a href=\"{}\">{name}</a>{divider}",
            diff_html::escape(&root)
        );
        let mut accumulated = String::new();
        for (index, part) in parts.iter().enumerate() {
            accumulated = format!("{accumulated}/{part}");
            if index + 1 == parts.len() {
                html.push_str(&diff_html::escape(part));
            } else {
                let href = self.uri(
                    action,
                    Link {
                        path: Some(&format!("{accumulated}/")),
                        ..Link::default()
                    },
                );
                let _ = write!(
                    html,
                    "<a href=\"{}\">{}</a>{divider}",
                    diff_html::escape(&href),
                    diff_html::escape(part)
                );
            }
        }
        html
    }

    /// The page's title: its parts and the repository's display name.
    fn title(&self, first: &str) -> String {
        format!("{first} · {}", self.repository.display_name())
    }

    fn shell<S: WebState>(&self, state: &S, viewer: &WebUser, title: &str) -> crate::Shell {
        crate::Shell::for_user(state.config(), viewer, title, &self.current)
    }
}

/// What a generated URI changes from the page's (`null` keeps it).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Link<'a> {
    path: Option<&'a str>,
    branch: Option<&'a str>,
    commit: Option<&'a str>,
    line: Option<&'a str>,
}

/// A `PHUITagView` of a header.
pub(crate) struct Tag {
    pub(crate) text: String,
    pub(crate) href: Option<String>,
    pub(crate) icon: Option<&'static str>,
    /// `TYPE_OUTLINE`, for branches.
    pub(crate) outline: bool,
}

/// A tab of `buildTabsView()`.
pub(crate) struct Tab {
    pub(crate) name: &'static str,
    pub(crate) icon: &'static str,
    pub(crate) href: String,
    pub(crate) selected: bool,
}

/// The pager of a list (`PHUIPagerView`): the links to the pages around
/// the page.
#[derive(Default)]
pub(crate) struct Pager {
    pub(crate) previous: Option<String>,
    pub(crate) next: Option<String>,
}

impl Pager {
    /// The links of a page at `offset` which found `found` results for a
    /// limit of one more than the page size; the results are cut to the
    /// page (`sliceResults()`).
    fn slice<T>(offset: i64, base: &str, results: &mut Vec<T>) -> Self {
        let size = usize::try_from(PAGE_SIZE).unwrap_or(usize::MAX);
        let more = results.len() > size;
        results.truncate(size);
        let link = |offset: i64| {
            if offset == 0 {
                base.to_owned()
            } else {
                format!("{base}?offset={offset}")
            }
        };
        Self {
            previous: (offset > 0).then(|| link((offset - PAGE_SIZE).max(0))),
            next: more.then(|| link(offset + PAGE_SIZE)),
        }
    }
}

/// `formatCommitName($hash, $local = true)`: the hash cut to 12
/// characters.
fn local_name(repository: &Repository, hash: &str) -> String {
    if repository.vcs == "git" || repository.vcs == "hg" {
        hash.get(..12).unwrap_or(hash).to_owned()
    } else {
        hash.to_owned()
    }
}

/// `PhutilUTF8StringTruncator` to `glyphs` characters, with an ellipsis.
fn truncate(text: &str, glyphs: usize) -> String {
    if text.chars().count() <= glyphs {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(glyphs - 1).collect();
    cut.push('…');
    cut
}

/// A `302` to `location`.
fn redirect(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(location) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

/// `DiffusionServeController::isVCSRequest()`: Git, Mercurial and
/// Subversion clients are served by PHP.
fn is_vcs_request(headers: &HeaderMap) -> bool {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    value("user-agent").starts_with("git/")
        || value("content-type").starts_with("application/x-git-")
        || value("content-type").starts_with("application/vnd.git-lfs+json")
        || value("x-phabricator-request-type") == "git-lfs"
        || headers.contains_key("dav")
}

/// The pager offset of a query string, the only parameter the pages read;
/// any other one is something PHP does (searches, views, blame...).
fn offset(query: Option<&str>) -> Result<i64, PageError> {
    let mut offset = 0;
    for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "offset" if value.is_empty() => {}
            "offset" if value.bytes().all(|byte| byte.is_ascii_digit()) => {
                offset = value
                    .parse()
                    .map_err(|_| PageError::Fallback("an unusual offset"))?;
            }
            _ => return Err(PageError::Fallback("a query string the pages don't read")),
        }
    }
    Ok(offset)
}

/// The page of a repository URI: its action, and the blob after it.
async fn dispatch<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    prefix: Prefix,
    identifier: &str,
    rest: &str,
    request: (Uri, HeaderMap, Option<String>),
) -> Result<Response, PageError> {
    let (uri, headers, query) = request;
    if is_vcs_request(&headers) {
        return Err(PageError::Fallback(
            "a request from a version control client",
        ));
    }
    // PHP's routes end each action with a slash.
    let Some((action, blob)) = rest.split_once('/').or(rest.is_empty().then_some(("", ""))) else {
        return Err(PageError::Fallback("a Diffusion page not ported"));
    };
    let action = match action {
        "" | "repository" if rest.is_empty() || action == "repository" => "repository",
        "browse" | "history" | "branches" | "tags" | "commit" => action,
        _ => return Err(PageError::Fallback("a Diffusion page not ported")),
    };
    if action == "commit" {
        // `commit/<hash>/branches/` and `/tags/` are fragments for PHP's
        // scripts; `commit/<hash>` is the commit page.
        let hash = blob.trim_end_matches('/');
        if hash.is_empty()
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(PageError::Fallback("a Diffusion page not ported"));
        }
        offset(query.as_deref())?;
        let context = match Context::load(&*state, &viewer, prefix, identifier, None, &uri).await {
            Ok(context) => context,
            Err(response) => return Ok(*response),
        };
        if prefix == Prefix::Source {
            return Ok(redirect(&context.repository.commit_uri(hash)));
        }
        return commit::page(&*state, &viewer, context, hash.to_owned()).await;
    }
    let blob = if blob.is_empty() {
        None
    } else {
        Some(uri::parse_blob(blob).ok_or(PageError::Fallback("a path PHP refuses"))?)
    };
    let offset = offset(query.as_deref())?;
    let context = match Context::load(&*state, &viewer, prefix, identifier, blob, &uri).await {
        Ok(context) => context,
        Err(response) => return Ok(*response),
    };
    match action {
        "repository" => home::page(&*state, &viewer, context, offset).await,
        "browse" => browse::page(&*state, &viewer, context, offset).await,
        "history" => history::page(&*state, &viewer, context, offset).await,
        "branches" => refs::branches(&*state, &viewer, context, offset).await,
        _ => refs::tags(&*state, &viewer, context, offset).await,
    }
}

/// `GET /source/{short}/`.
pub(crate) async fn source_root<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(short): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    dispatch(
        state,
        viewer,
        Prefix::Source,
        &short,
        "",
        (uri, headers, query),
    )
    .await
}

/// `GET /source/{short}/{rest}`.
pub(crate) async fn source<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((short, rest)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    dispatch(
        state,
        viewer,
        Prefix::Source,
        &short,
        &rest,
        (uri, headers, query),
    )
    .await
}

/// `GET /diffusion/{identifier}/`.
pub(crate) async fn diffusion_root<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(identifier): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    dispatch(
        state,
        viewer,
        Prefix::Diffusion,
        &identifier,
        "",
        (uri, headers, query),
    )
    .await
}

/// `GET /diffusion/{identifier}/{rest}`.
pub(crate) async fn diffusion<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((identifier, rest)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    dispatch(
        state,
        viewer,
        Prefix::Diffusion,
        &identifier,
        &rest,
        (uri, headers, query),
    )
    .await
}
