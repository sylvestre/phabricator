//! `/D<id>`: a revision (`DifferentialRevisionViewController`, W4), with
//! one of its diffs (`?id=`, the newest by default) or its raw diff
//! (`?download`, [`crate::raw_diff`]), and `/D<id>/new/`, the changes since
//! the viewer's last action. Comparisons between two diffs (`?vs=`) are
//! left to PHP.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_differential::diff_search::{self, VisibleDiff};
use phab_app_differential::repository::{self, Repository};
use phab_app_differential::storage::{self, Revision};
use phab_app_differential::{details, merge_conflict};
use phab_app_project as project;
use phab_core::{Config, Phid, PhidType};
use phab_db::edges;
use phab_policy::{Viewer, application};
use phab_remarkup::{Markup, Mode};

use crate::error::PageError;
use crate::response::Page;
use crate::searchfox::Tree;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, remarkup, time};

/// A linked name.
#[derive(Clone)]
pub(crate) struct Link {
    name: String,
    uri: Option<String>,
    closed: bool,
}

/// A reviewer and what they did.
pub(crate) struct ReviewerItem {
    phid: String,
    link: Link,
    status: &'static str,
    blocking: bool,
}

/// The rows of `DifferentialMergeConflictStatusField`
/// (`renderPropertyViewValue()`).
pub(crate) struct MergeConflictView {
    /// The verdict, for the class.
    verdict: &'static str,
    /// The tooltip of the verdict.
    label: &'static str,
    text: &'static str,
    note: Option<String>,
    /// "Last checked", and what the check ran against.
    last_checked: Option<(String, Option<String>)>,
    hint: Option<String>,
}

impl MergeConflictView {
    fn new(view: merge_conflict::PropertyView) -> Self {
        let last_checked = view.last_checked.map(|epoch| {
            let target = match epoch {
                Some(epoch) => format!("Last checked {}", time::format_utc(epoch)),
                None => "Last checked at an unrecorded time".to_owned(),
            };
            (target, view.checked_against)
        });
        Self {
            verdict: view.verdict.key(),
            label: view.verdict.label(),
            text: view.verdict.text(),
            note: view.verdict_note,
            last_checked,
            hint: view.hint,
        }
    }
}

/// A warning above the revision (`DifferentialRevisionWarning`).
pub(crate) struct Warning {
    title: String,
    message_html: &'static str,
}

#[derive(Template)]
#[template(path = "revision.html")]
pub(crate) struct RevisionPage {
    shell: Shell,
    warnings: Vec<Warning>,
    actions: &'static [(&'static str, &'static str)],
    monogram: String,
    title: String,
    status: &'static str,
    closed: bool,
    author: Link,
    created: String,
    modified: String,
    reviewers: Vec<ReviewerItem>,
    /// The subscribers, with their PHIDs to remove them.
    subscribers: Vec<(String, Link)>,
    /// The "Subscribe" or "Unsubscribe" action: where it posts and its
    /// label. The author and the reviewers are subscribed automatically.
    subscription: Option<(String, &'static str)>,
    bug: Option<(i64, String)>,
    merge_conflict: Option<MergeConflictView>,
    parents: Vec<Link>,
    children: Vec<Link>,
    lando_uri: Option<String>,
    /// The repository's name, and its Searchfox tree if it has one.
    repository: Option<(String, Option<String>)>,
    summary: Option<Markup>,
    test_plan: Option<Markup>,
    diff_id: Option<i64>,
    changesets: Vec<String>,
    timeline: Vec<crate::timeline::Entry>,
    /// The builds, lint and unit results of the active diff.
    checks: crate::harbormaster::DiffChecks,
}

impl RevisionPage {
    fn html(markup: Option<&Markup>) -> &str {
        markup.map_or("", |markup| {
            std::str::from_utf8(markup.as_bytes()).unwrap_or("")
        })
    }

    fn summary_html(&self) -> &str {
        Self::html(self.summary.as_ref())
    }

    fn test_plan_html(&self) -> &str {
        Self::html(self.test_plan.as_ref())
    }
}

/// `DifferentialRevisionStatus` names.
pub(crate) fn status_name(status: &str) -> &'static str {
    match status {
        "needs-review" => "Needs Review",
        "needs-revision" => "Needs Revision",
        "changes-planned" => "Changes Planned",
        "accepted" => "Accepted",
        "published" => "Closed",
        "abandoned" => "Abandoned",
        "draft" => "Draft",
        _ => "Unknown",
    }
}

/// `DifferentialReviewerStatus` as the reviewer list shows it.
fn reviewer_status(status: &str) -> &'static str {
    match status {
        "accepted" => "Accepted",
        "rejected" => "Requested Changes",
        "commented" => "Commented",
        "accepted-older" => "Accepted Prior Diff",
        "rejected-older" => "Requested Changes to Prior Diff",
        "resigned" => "Resigned",
        _ => "Review Requested",
    }
}

/// Names for users and projects, under the viewer's policy. Anything else,
/// or anything the viewer can't see, is a restricted handle like PHP's.
async fn links<S: WebState>(
    state: &S,
    viewer: &Viewer,
    phids: &[Phid],
) -> Result<Vec<(Phid, Link)>, PageError> {
    let users: Vec<Phid> = phids
        .iter()
        .filter(|phid| phid.phid_type() == PhidType::USER)
        .cloned()
        .collect();
    let projects: Vec<Phid> = phids
        .iter()
        .filter(|phid| phid.phid_type() == PhidType::PROJECT)
        .cloned()
        .collect();
    let mut found: Vec<(Phid, Link)> =
        phab_auth::user::load_users_by_phid(state.database(), &users)
            .await?
            .into_iter()
            .map(|user| {
                let link = Link {
                    uri: Some(format!("/p/{}/", user.user_name)),
                    closed: user.is_disabled,
                    name: user.user_name,
                };
                (user.phid, link)
            })
            .collect();
    let loaded = project::storage::load_by_phid(state.database(), &projects).await?;
    let (visible, graph) = project::policy::visible_projects(
        state.database(),
        state.config(),
        viewer,
        loaded,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    found.extend(visible.iter().map(|project| {
        (
            project.phid.clone(),
            Link {
                name: format!("#{}", project.display_name(graph.parent(project))),
                uri: Some(project.uri()),
                closed: project.is_archived(),
            },
        )
    }));
    Ok(phids
        .iter()
        .map(|phid| {
            let link = found
                .iter()
                .position(|(found, _)| found == phid)
                .map_or_else(
                    || Link {
                        name: "Restricted Object".to_owned(),
                        uri: None,
                        closed: false,
                    },
                    |index| found.swap_remove(index).1,
                );
            (phid.clone(), link)
        })
        .collect())
}

async fn revision_links<S: WebState>(
    state: &S,
    viewer: &Viewer,
    phids: &[Phid],
) -> Result<Vec<Link>, PageError> {
    let revisions = storage::load_revisions(state.database(), phids).await?;
    let mut revisions = storage::visible_revisions(
        state.database(),
        state.config(),
        viewer,
        revisions,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    revisions.sort_by_key(|revision| revision.id);
    Ok(revisions
        .into_iter()
        .map(|revision| Link {
            name: format!("D{}: {}", revision.id, revision.title),
            uri: Some(format!("/D{}", revision.id)),
            closed: revision.is_closed(),
        })
        .collect())
}

/// `LandoLinkEventListener` (moz-extensions): repositories tagged with the
/// "lando" project use `lando.url`, the others `lando-ui.url`.
async fn lando_uri(
    config: &Config,
    database: &phab_db::Database,
    revision: &Revision,
) -> Result<Option<String>, PageError> {
    let legacy = config.nonempty_string("lando-ui.url");
    let new = config.nonempty_string("lando.url");
    if legacy.is_none() && new.is_none() {
        return Ok(None);
    }
    let Some(repository) = &revision.repository_phid else {
        return Ok(None);
    };
    let project_phids = edges::load_destination_phids(
        database,
        std::slice::from_ref(repository),
        &[edges::OBJECT_HAS_PROJECT],
        None,
    )
    .await?;
    let uses_new = project::storage::load_by_phid(database, &project_phids)
        .await?
        .iter()
        .any(|project| project.name.eq_ignore_ascii_case("lando"));
    let base = if uses_new { new } else { legacy };
    Ok(base.map(|base| {
        let origin = base.trim_end_matches('/');
        let origin = origin
            .find("://")
            .and_then(|scheme| {
                origin[scheme + 3..]
                    .find('/')
                    .map(|path| &origin[..scheme + 3 + path])
            })
            .unwrap_or(origin);
        format!("{origin}/D{}/", revision.id)
    }))
}

/// The project marking secure revisions, if the viewer can see it.
async fn secure_project<S: WebState>(
    state: &S,
    viewer: &Viewer,
) -> Result<Option<Phid>, PageError> {
    let found = project::storage::load_by_name(state.database(), "secure-revision").await?;
    let (visible, _) = project::policy::visible_projects(
        state.database(),
        state.config(),
        viewer,
        found,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    Ok(visible.into_iter().next().map(|project| project.phid))
}

/// `isRevisionPrivate()`: tagged with the "secure-revision" project.
async fn is_private(
    database: &phab_db::Database,
    revision: &Phid,
    secure: &Phid,
) -> Result<bool, PageError> {
    let projects = edges::load_destination_phids(
        database,
        std::slice::from_ref(revision),
        &[edges::OBJECT_HAS_PROJECT],
        Some(std::slice::from_ref(secure)),
    )
    .await?;
    Ok(!projects.is_empty())
}

/// `DifferentialRevisionWarning`: a secure revision says how to manage it,
/// and warns when open public revisions depend on it.
async fn warnings<S: WebState>(
    state: &S,
    viewer: &Viewer,
    revision: &Revision,
) -> Result<Vec<Warning>, PageError> {
    let database = state.database();
    let Some(secure) = secure_project(state, viewer).await? else {
        return Ok(Vec::new());
    };
    if !is_private(database, &revision.phid, &secure).await? {
        return Ok(Vec::new());
    }
    let mut warnings = vec![Warning {
        title: "This is a secure revision.".to_owned(),
        message_html: "Please use the CC list of the associated bug in Bugzilla to manage access and \
             subscribership of this revision. Changes made here may be overwritten.<br/>Please do not \
             land this revision using `arc land` to prevent data leakage.",
    }];

    // Every revision reachable through "depended on by" edges.
    let mut seen = vec![revision.phid.clone()];
    let mut frontier = vec![revision.phid.clone()];
    while !frontier.is_empty() {
        let children = edges::load_destination_phids(
            database,
            &frontier,
            &[edges::REVISION_DEPENDED_ON_BY_REVISION],
            None,
        )
        .await?;
        frontier = children
            .into_iter()
            .filter(|child| !seen.contains(child))
            .collect();
        seen.extend(frontier.iter().cloned());
    }
    let reachable = storage::load_revisions(database, &seen[1..]).await?;
    let reachable =
        storage::visible_revisions(database, state.config(), viewer, reachable, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    let open: Vec<Phid> = reachable
        .iter()
        .filter(|child| !child.is_closed())
        .map(|child| child.phid.clone())
        .collect();
    let private: std::collections::HashSet<Phid> = edges::load_edges(
        database,
        &open,
        &[edges::OBJECT_HAS_PROJECT],
        Some(std::slice::from_ref(&secure)),
    )
    .await?
    .into_iter()
    .map(|edge| edge.src)
    .collect();
    let public = open
        .iter()
        .filter(|&child| !private.contains(child))
        .count();
    if public > 0 {
        let plural = if public > 1 { "s" } else { "" };
        warnings.push(Warning {
            title: format!("A public revision{plural} is depending on this secure revision."),
            message_html: if public > 1 {
                "Please change the child revisions to private by setting the security flag in Bugzilla."
            } else {
                "Please change the child revision to private by setting the security flag in Bugzilla."
            },
        });
    }
    Ok(warnings)
}

/// The `diff` program realignment runs: `PHAB_DIFF_COMMAND`, like
/// `busybox diff`, as PHP's image runs the `BusyBox` one, whose alignment can
/// differ from GNU diff's.
pub(crate) fn diff_command() -> phab_diff::realign::DiffCommand {
    static COMMAND: std::sync::OnceLock<phab_diff::realign::DiffCommand> =
        std::sync::OnceLock::new();
    COMMAND
        .get_or_init(|| {
            std::env::var("PHAB_DIFF_COMMAND")
                .ok()
                .and_then(|command| phab_diff::realign::DiffCommand::parse(&command))
                .unwrap_or_default()
        })
        .clone()
}

/// The inline comments on these changesets the viewer may see, ready to
/// show, by changeset.
pub(crate) async fn inlines<S: WebState>(
    state: &S,
    viewer: &Viewer,
    changesets: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<diff_html::Inline>>, PageError> {
    let database = state.database();
    let comments = storage::load_inline_comments(database, changesets).await?;
    let subjects: Vec<_> = comments
        .iter()
        .map(storage::InlineComment::subject)
        .collect();
    let visible =
        storage::visible_subjects(database, state.config(), viewer, &subjects, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    let comments: Vec<_> = comments
        .into_iter()
        .zip(visible)
        .filter_map(|(comment, visible)| visible.then_some(comment))
        .collect();
    let authors: Vec<Phid> = comments
        .iter()
        .map(|comment| comment.author_phid.clone())
        .collect();
    let users = phab_auth::user::load_users_by_phid(database, &authors).await?;
    let texts: Vec<&str> = comments
        .iter()
        .map(|comment| comment.content.as_str())
        .collect();
    let bodies = remarkup::render_all(state, viewer, &texts, Mode::Html).await?;
    let mut by_changeset: std::collections::HashMap<i64, Vec<diff_html::Inline>> =
        std::collections::HashMap::new();
    for (comment, body) in comments.iter().zip(bodies) {
        let author = users
            .iter()
            .find(|user| user.phid == comment.author_phid)
            .map_or_else(
                || "Unknown User".to_owned(),
                |user| {
                    format!(
                        "<a href=\"/p/{0}/\">{0}</a>",
                        diff_html::escape(&user.user_name)
                    )
                },
            );
        by_changeset
            .entry(comment.changeset_id)
            .or_default()
            .push(diff_html::Inline {
                phid: comment.phid.to_string(),
                is_new_file: comment.is_new_file,
                last_line: comment.line_number + comment.line_length,
                author_html: format!(
                    "{author} <time class=\"muted\">{}</time>",
                    time::format_utc(comment.date_created)
                ),
                body_html: String::from_utf8_lossy(body.as_bytes()).into_owned(),
            });
    }
    Ok(by_changeset)
}

/// The revision's repository, if the viewer can see it.
async fn repository<S: WebState>(
    state: &S,
    viewer: &Viewer,
    revision: &Revision,
) -> Result<Option<Repository>, PageError> {
    let Some(phid) = &revision.repository_phid else {
        return Ok(None);
    };
    let found = repository::load(state.database(), std::slice::from_ref(phid)).await?;
    let mut visible =
        repository::visible(state.database(), state.config(), viewer, found, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    Ok(visible.pop())
}

/// The changesets of a diff, rendered with their inline comments.
pub(crate) async fn changesets<S: WebState>(
    state: &S,
    viewer: &Viewer,
    diff_phid: &Phid,
    searchfox: Option<Tree>,
) -> Result<(Option<i64>, Vec<String>), PageError> {
    let database = state.database();
    let internal = |error: phab_diff::storage::StorageError| PageError::Internal(Box::new(error));
    let Some(diff) = phab_diff::storage::load_diff_by_phid(database, diff_phid.as_str())
        .await
        .map_err(internal)?
    else {
        return Ok((None, Vec::new()));
    };
    let mut changesets = phab_diff::storage::load_changesets(database, diff.id)
        .await
        .map_err(internal)?;
    phab_diff::sort_for_display(&mut changesets);
    let ids: Vec<i64> = changesets.iter().map(|changeset| changeset.id).collect();
    let mut inlines = inlines(state, viewer, &ids).await?;
    let options = phab_diff::view::ViewOptions {
        diff_command: diff_command(),
        ..phab_diff::view::ViewOptions::default()
    };
    // Building a view runs `diff` and the intraline edit distance, which
    // would hold a runtime thread for as long as they take.
    let rendered = tokio::task::spawn_blocking(move || {
        changesets
            .iter()
            .map(|changeset| {
                let inlines = inlines.remove(&changeset.id).unwrap_or_default();
                match phab_diff::view::build(changeset, &options) {
                    Ok(view) => {
                        diff_html::changeset_with_inlines(&view, &inlines, searchfox.as_ref())
                    }
                    Err(error) => {
                        tracing::warn!(%error, changeset = changeset.id, "changeset failed to render");
                        "<section class=\"changeset\"><p class=\"changeset-shield\">This file could not be shown.</p></section>"
                            .to_owned()
                    }
                }
            })
            .collect()
    })
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    Ok((Some(diff.id), rendered))
}

/// The ID of `D123`; `D0123` is not a monogram.
pub(crate) fn parse_monogram(monogram: &str) -> Option<i64> {
    monogram
        .strip_prefix('D')
        .filter(|digits| {
            !digits.is_empty()
                && !digits.starts_with('0')
                && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|digits| digits.parse().ok())
}

/// A Remarkup field, if not empty.
async fn remarkup_field<S: WebState>(
    state: &S,
    viewer: &Viewer,
    text: &str,
) -> Result<Option<Markup>, PageError> {
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        remarkup::render(state, viewer, text, Mode::Html).await?,
    ))
}

/// The author and the reviewers, in the order PHP lists them.
async fn people<S: WebState>(
    state: &S,
    viewer: &Viewer,
    revision: &Revision,
) -> Result<(Link, Vec<ReviewerItem>, Vec<(String, Link)>), PageError> {
    let subscriber_phids = edges::load_destination_phids(
        state.database(),
        std::slice::from_ref(&revision.phid),
        &[edges::OBJECT_HAS_SUBSCRIBER],
        None,
    )
    .await?;
    let mut phids: Vec<Phid> = revision
        .reviewers
        .iter()
        .map(|reviewer| reviewer.phid.clone())
        .chain(subscriber_phids.iter().cloned())
        .collect();
    phids.sort();
    phids.dedup();
    if let Some(author) = &revision.author_phid {
        phids.push(author.clone());
    }
    let links = links(state, viewer, &phids).await?;
    let author = revision
        .author_phid
        .as_ref()
        .and_then(|author| links.iter().position(|(phid, _)| phid == author))
        .map_or_else(
            || Link {
                name: "Unknown".to_owned(),
                uri: None,
                closed: false,
            },
            |index| links[index].1.clone(),
        );
    let link = |phid: &Phid| {
        links
            .iter()
            .find(|(linked, _)| linked == phid)
            .map(|(_, link)| link.clone())
    };
    let reviewers = revision
        .reviewers
        .iter()
        .filter_map(|reviewer| {
            Some(ReviewerItem {
                phid: reviewer.phid.to_string(),
                link: link(&reviewer.phid)?,
                status: reviewer_status(&reviewer.status),
                blocking: reviewer.status == "blocking",
            })
        })
        .collect();
    let subscribers = subscriber_phids
        .iter()
        .filter_map(|phid| Some((phid.to_string(), link(phid)?)))
        .collect();
    Ok((author, reviewers, subscribers))
}

/// The "Subscribe" or "Unsubscribe" action of the viewer
/// (`PhabricatorSubscriptionsUIEventListener`), none for the author and
/// the reviewers who didn't resign (`isAutomaticallySubscribed()`).
fn subscription(
    viewer: &WebUser,
    revision: &Revision,
    subscribers: &[(String, Link)],
) -> Option<(String, &'static str)> {
    let phid = &viewer.user.phid;
    let is_automatic = revision.author_phid.as_ref() == Some(phid)
        || revision
            .reviewers
            .iter()
            .any(|reviewer| reviewer.phid == *phid && reviewer.status != "resigned");
    if is_automatic {
        return None;
    }
    let revision_phid = &revision.phid;
    if subscribers.iter().any(|(known, _)| known == phid.as_str()) {
        Some((
            format!("/subscriptions/delete/{revision_phid}/"),
            "Unsubscribe",
        ))
    } else {
        Some((format!("/subscriptions/add/{revision_phid}/"), "Subscribe"))
    }
}

/// `GET /{monogram}`: path parameters can't share a segment with text, so
/// revisions (`D123`), buildables (`B123`), Herald rules (`H123`) and
/// commits (`rXYZabc`, `R12:abc`) share the route.
pub(crate) async fn by_monogram<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(monogram): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    query: RawQuery,
) -> Result<Response, PageError> {
    if crate::diffusion::parse_commit_monogram(&monogram).is_some() {
        return crate::diffusion::commit(State(state), viewer, Path(monogram), uri, headers, query)
            .await;
    }
    if let Some(id) = crate::harbormaster::parse_buildable_monogram(&monogram) {
        return Ok(crate::harbormaster::buildable(state, viewer, id, uri)
            .await?
            .into_response());
    }
    if let Some(id) = crate::dashboards::parse_panel_monogram(&monogram) {
        return Ok(crate::dashboards::panel(state, viewer, id, uri)
            .await?
            .into_response());
    }
    if let Some(id) = crate::files::parse_monogram(&monogram) {
        return Ok(crate::files::show(state, viewer, id, uri)
            .await?
            .into_response());
    }
    if let Some(id) = crate::herald::parse_rule_monogram(&monogram) {
        return Ok(crate::herald::rule(state, viewer, id, uri)
            .await?
            .into_response());
    }
    if let Some(id) = monogram
        .strip_prefix('S')
        .filter(|digits| !digits.starts_with('0'))
        .and_then(|digits| crate::system::parse_id(digits).ok())
    {
        return Ok(crate::system::spaces::view(&state, viewer, id, uri)
            .await?
            .into_response());
    }
    show(
        state,
        viewer,
        (monogram, false),
        uri,
        &headers,
        query.0.as_deref(),
    )
    .await
}

/// The parameters of the revision page (`getInt('id')`, `getInt('vs')`,
/// `getExists('download')`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewParams {
    /// The diff to show.
    id: Option<i64>,
    /// The diff to compare it with.
    vs: Option<i64>,
    /// "Download Raw Diff".
    download: bool,
}

/// `getInt()` of a parameter: `(int)` of plain integers; other values PHP
/// would cast in ways not worth reproducing are left to it.
fn int_param(value: &str) -> Result<i64, PageError> {
    if value.is_empty() {
        return Ok(0);
    }
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageError::Fallback("unusual diff IDs"));
    }
    value
        .parse()
        .map_err(|_| PageError::Fallback("unusual diff IDs"))
}

impl ViewParams {
    /// The parameters of a query string; others are ignored, as PHP does.
    pub(crate) fn parse(query: Option<&str>) -> Result<Self, PageError> {
        let mut params = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
            match key.as_ref() {
                "id" => params.id = Some(int_param(&value)?),
                "vs" => params.vs = Some(int_param(&value)?),
                "download" => params.download = true,
                // `id[]=` and the like are arrays to PHP.
                key if key.starts_with("id[") || key.starts_with("vs[") => {
                    return Err(PageError::Fallback("unusual diff IDs"));
                }
                _ => {}
            }
        }
        Ok(params)
    }
}

/// The notices of `/D<id>/new/` (`PHUIInfoView`).
const NO_ACTIONS: (&str, &str) = (
    "No Actions",
    "Showing all changes because you have never taken an action on this revision.",
);
const NO_RECENT_UPDATES: (&str, &str) = (
    "No Recent Updates",
    "Showing all changes because the diff for this revision has not been updated since your \
     last action.",
);

/// `DifferentialRevisionUpdateTransaction::TRANSACTIONTYPE`.
const UPDATE_TRANSACTION: &str = "differential:update";

/// `getOldDiffID()` of `/D<id>/new/`: the diff that was active when the
/// viewer last took an action, or the notice saying why all changes are
/// shown.
async fn since_last_action<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: &Revision,
    diffs: &[VisibleDiff],
) -> Result<(Option<i64>, Option<(&'static str, &'static str)>), PageError> {
    let transactions = storage::load_object_transactions(state.database(), &revision.phid).await?;
    // The viewer's newest transaction, which they can always see.
    let Some(last_action) = transactions
        .iter()
        .rev()
        .find(|(transaction, _)| transaction.author_phid == viewer.user.phid)
        .map(|(_, date)| *date)
    else {
        return Ok((None, Some(NO_ACTIONS)));
    };
    let mut older = std::collections::HashSet::new();
    let mut has_newer = false;
    for (transaction, date) in &transactions {
        if transaction.transaction_type != UPDATE_TRANSACTION {
            continue;
        }
        if *date <= last_action {
            older.insert(phab_core::php::string_cast(&transaction.new_value).into_owned());
        } else {
            has_newer = true;
        }
    }
    if !has_newer {
        return Ok((None, Some(NO_RECENT_UPDATES)));
    }
    // The most recent diff from before the last action.
    let old = diffs
        .iter()
        .take_while(|diff| older.contains(diff.phid.as_str()))
        .last();
    Ok((old.map(|diff| diff.id), None))
}

/// `GET /D{id}` and `GET /D{id}/new/` (`filter_new`): the revision, one of
/// its diffs (`?id=`) or its raw diff (`?download`). Comparisons between
/// diffs (`?vs=`, and what `/new/` shows when the diff changed since the
/// viewer's last action) are left to PHP.
pub(crate) async fn show<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    (monogram, filter_new): (String, bool),
    uri: Uri,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Response, PageError> {
    viewer
        .require_application(&*state, application::DIFFERENTIAL)
        .await?;
    let id = parse_monogram(&monogram).ok_or(PageError::NotFound)?;
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    let revision = storage::load_revisions_by_id(database, &[id])
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    let revision = storage::visible_revisions(
        database,
        state.config(),
        &as_viewer,
        vec![revision],
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?
    .pop()
    .ok_or(PageError::PolicyDenied)?;

    let params = ViewParams::parse(query)?;
    let scope = crate::harbormaster::Scope::new(state.as_ref(), &viewer);
    let diffs = diff_search::revision_diffs(&scope.request(), revision.id)
        .await
        .map_err(crate::harbormaster::method_error)?;
    // PHP throws ("This revision has no diffs."); the page shows the
    // revision without changesets.
    let last = diffs.last();
    let has_diff = |id: i64| diffs.iter().any(|diff| diff.id == id);
    let is_set = |value: Option<i64>| value.is_some_and(|value| value != 0);
    // `getOldDiffID()`, then `getNewDiffID()`.
    let mut notice = None;
    let old_id = if filter_new {
        if is_set(params.vs) {
            return Err(PageError::NotFound);
        }
        let (old_id, new_notice) =
            since_last_action(state.as_ref(), &viewer, &revision, &diffs).await?;
        notice = new_notice;
        old_id
    } else {
        params.vs
    };
    if filter_new && is_set(params.id) {
        return Err(PageError::NotFound);
    }
    let target = params
        .id
        .filter(|id| *id != 0 && has_diff(*id))
        .and_then(|id| diffs.iter().find(|diff| diff.id == id))
        .or(last);
    if old_id.is_some_and(|id| id != 0 && has_diff(id)) {
        return Err(PageError::Fallback("comparisons between diffs"));
    }
    if params.download {
        let target = target.ok_or(PageError::Fallback("revisions without diffs"))?;
        return crate::raw_diff::download(
            state.as_ref(),
            &viewer,
            (&revision, target),
            uri.path(),
            headers,
        )
        .await;
    }
    let diff_phid = target.map(|diff| &diff.phid);
    let mut page = page(state.as_ref(), &viewer, revision, diff_phid, &uri).await?;
    if let Some((title, message_html)) = notice {
        page.warnings.insert(
            0,
            Warning {
                title: title.to_owned(),
                message_html,
            },
        );
    }
    Ok(Page(page).into_response())
}

/// `GET /D{id}/new/`.
pub(crate) async fn show_new<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(monogram): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, PageError> {
    show(
        state,
        viewer,
        (monogram, true),
        uri,
        &headers,
        query.as_deref(),
    )
    .await
}

/// The revision page, showing the changesets of `diff_phid`.
async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: Revision,
    diff_phid: Option<&Phid>,
    uri: &Uri,
) -> Result<RevisionPage, PageError> {
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    let details = details::load_details(database, &revision.phid)
        .await?
        .ok_or(PageError::NotFound)?;
    let bug = details::load_bug_id(database, &revision.phid)
        .await?
        .map(|bug| {
            let base = phab_remarkup::phabricator::bugzilla_origin(
                state.config().nonempty_string("bugzilla.url").unwrap_or(""),
            );
            (bug, format!("{base}/show_bug.cgi?id={bug}"))
        });
    let (parent_phids, child_phids) = details::load_stack_edges(database, &revision.phid).await?;
    let scope = crate::harbormaster::Scope::new(state, viewer);
    let merge_conflict = merge_conflict::property_view(&scope.request(), &revision)
        .await
        .map_err(crate::harbormaster::method_error)?
        .map(MergeConflictView::new);

    let (author, reviewers, subscribers) = people(state, &as_viewer, &revision).await?;
    let summary = remarkup_field(state, &as_viewer, &details.summary).await?;
    let test_plan = remarkup_field(state, &as_viewer, &details.test_plan).await?;
    let repository = repository(state, &as_viewer, &revision).await?;
    let searchfox = repository
        .as_ref()
        .and_then(|repository| state.searchfox_trees().tree_for(repository));
    let (diff_id, changesets) = match diff_phid {
        Some(diff) => changesets(state, &as_viewer, diff, searchfox.clone()).await?,
        None => (None, Vec::new()),
    };

    let subscription = subscription(viewer, &revision, &subscribers);
    let monogram = format!("D{}", revision.id);
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    let warnings = warnings(state, &as_viewer, &revision).await?;
    let timeline = crate::timeline::entries(state, &as_viewer, &revision).await?;
    let checks = match &details.active_diff_phid {
        Some(diff) => crate::harbormaster::diff_checks(state, viewer, &revision.phid, diff).await?,
        None => crate::harbormaster::DiffChecks::default(),
    };
    Ok(RevisionPage {
        warnings,
        actions: crate::actions::ACTIONS,
        shell: Shell::for_user(
            state.config(),
            viewer,
            &format!("{monogram} {}", revision.title),
            current,
        ),
        title: revision.title.clone(),
        status: status_name(&revision.status),
        closed: revision.is_closed(),
        author,
        created: time::format_utc(details.date_created),
        modified: time::format_utc(details.date_modified),
        reviewers,
        subscribers,
        subscription,
        bug,
        merge_conflict,
        parents: revision_links(state, &as_viewer, &parent_phids).await?,
        children: revision_links(state, &as_viewer, &child_phids).await?,
        lando_uri: lando_uri(state.config(), database, &revision).await?,
        repository: repository
            .map(|repository| (repository.name, searchfox.as_ref().map(Tree::uri))),
        summary,
        test_plan,
        diff_id,
        changesets,
        timeline,
        checks,
        monogram,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_parameters_are_read_like_php() {
        assert_eq!(
            ViewParams::parse(Some("id=12&vs=&download")).unwrap(),
            ViewParams {
                id: Some(12),
                vs: Some(0),
                download: true,
            }
        );
        assert_eq!(
            ViewParams::parse(Some("vs=-1&x=y")).unwrap(),
            ViewParams {
                id: None,
                vs: Some(-1),
                download: false,
            }
        );
        assert_eq!(ViewParams::parse(None).unwrap(), ViewParams::default());
        assert!(ViewParams::parse(Some("id=1x")).is_err());
        assert!(ViewParams::parse(Some("id[]=1")).is_err());
    }
}
