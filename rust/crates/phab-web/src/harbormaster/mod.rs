//! Harbormaster pages (`PhabricatorHarbormasterApplication`), read-only:
//! buildables (`/B<id>`), builds, build plans and the lists of buildables
//! and plans, and the build status of the revision page. The commands
//! (restart, pause, plan edits...) link to PHP's workflows.
//!
//! Objects are loaded with the queries of the `harbormaster.*.search`
//! methods, as the viewer, so a page shows what PHP would. What those
//! queries leave to PHP (buildables of objects other than diffs and
//! commits...) sends the page to PHP too.

mod build;
mod buildable;
mod lists;
mod log;
mod plan;
mod revision;
pub(crate) mod status;
mod step;
mod unit;

use std::collections::{HashMap, HashSet};

use phab_app_differential::{commit, diff_search, repository, storage as differential};
use phab_app_harbormaster::web::{
    self as harbormaster, Build, Plan, Transaction, TransactionTable,
};
use phab_app_project as project;
use phab_auth::User;
use phab_conduit::{ApiRequest, Context, MethodError};
use phab_core::{Phid, PhidType};
use phab_policy::Viewer;

pub(crate) use build::{show as build, show_run as build_run};
pub(crate) use buildable::{parse_monogram as parse_buildable_monogram, show as buildable};
pub(crate) use lists::{
    build_query, buildables as buildable_list, builds as build_list, plan_query, plans as plan_list,
};
pub(crate) use log::show as log;
pub(crate) use plan::show as plan;
pub(crate) use revision::{DiffChecks, diff_checks};
pub(crate) use step::show as step;

use crate::error::PageError;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;
use crate::{diff_html, time};

/// `PhabricatorHarbormasterApplication`.
pub(crate) const APPLICATION: &str = phab_app_harbormaster::APPLICATION.class;

/// The viewer of a page as the Harbormaster queries take them: a Conduit
/// request made by the logged-in user.
pub(crate) struct Scope {
    context: Context,
    user: User,
    now: i64,
}

impl Scope {
    pub(crate) fn new<S: WebState>(state: &S, viewer: &WebUser) -> Self {
        Self {
            context: Context {
                database: state.database().clone(),
                config: state.config().clone(),
                hostname: String::new(),
            },
            user: viewer.user.clone(),
            now: state.now(),
        }
    }

    pub(crate) fn request(&self) -> ApiRequest<'_> {
        self.request_with(serde_json::Map::new())
    }

    /// A request with these parameters, to call a Conduit method as the
    /// viewer.
    pub(crate) fn request_with(
        &self,
        params: serde_json::Map<String, serde_json::Value>,
    ) -> ApiRequest<'_> {
        ApiRequest {
            context: &self.context,
            user: Some(self.user.clone()),
            viewer: self.user.viewer(),
            params,
            strictly_typed: true,
            now: self.now,
            writes: phab_auth::Writes::Skipped,
        }
    }

    pub(crate) fn viewer(&self) -> Viewer {
        self.user.viewer()
    }
}

/// A query which needs what isn't ported goes to PHP.
pub(crate) fn method_error(error: MethodError) -> PageError {
    match error {
        MethodError::Unsupported(reason) => PageError::Fallback(reason),
        other => PageError::Internal(Box::new(std::io::Error::other(format!("{other:?}")))),
    }
}

pub(crate) fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

/// Whether the viewer may use an application, as
/// `PhabricatorApplication::isClassInstalledForViewer()` says.
pub(crate) async fn can_use<S: WebState>(
    state: &S,
    viewer: &WebUser,
    class: &'static str,
) -> Result<bool, PageError> {
    match viewer.require_application(state, class).await {
        Ok(()) => Ok(true),
        Err(PageError::NotFound | PageError::PolicyDenied) => Ok(false),
        Err(other) => Err(other),
    }
}

/// An object handle (`PhabricatorObjectHandle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handle {
    /// `getName()`.
    pub(crate) name: String,
    /// `getFullName()`.
    pub(crate) full_name: String,
    /// `getURI()`; `None` for objects the viewer can't see.
    pub(crate) uri: Option<String>,
    /// Closed objects, like disabled users, are struck.
    pub(crate) closed: bool,
}

impl Handle {
    fn new(name: String, full_name: String, uri: String) -> Self {
        Self {
            name,
            full_name,
            uri: Some(uri),
            closed: false,
        }
    }

    fn plain(name: String, uri: String) -> Self {
        Self::new(name.clone(), name, uri)
    }

    /// `renderLink()`: the full name (users show their username).
    pub(crate) fn link_html(&self) -> String {
        link_html(&self.full_name, self.uri.as_deref(), self.closed)
    }

    /// A link showing `getName()`.
    pub(crate) fn short_link_html(&self) -> String {
        link_html(&self.name, self.uri.as_deref(), self.closed)
    }
}

/// A link, or the text alone without a URI.
pub(crate) fn link_html(text: &str, uri: Option<&str>, closed: bool) -> String {
    let text = diff_html::escape(text);
    match uri {
        Some(uri) => format!(
            "<a href=\"{}\"{}>{text}</a>",
            diff_html::escape(uri),
            if closed {
                " class=\"remarkup-object-closed\""
            } else {
                ""
            }
        ),
        None => format!("<span class=\"handle-restricted\">{text}</span>"),
    }
}

/// `getTypeName()` of the PHID types the pages name.
fn type_name(phid: &Phid) -> Option<&'static str> {
    Some(match phid.as_str().get(5..9)? {
        "USER" => "User",
        "PROJ" => "Project",
        "DREV" => "Differential Revision",
        "DIFF" => "Differential Diff",
        "CMIT" => "Diffusion Commit",
        "REPO" => "Repository",
        "HMBB" => "Buildable",
        "HMBD" => "Build",
        "HMCP" => "Build Plan",
        "FILE" => "File",
        "APPS" => "Application",
        _ => return None,
    })
}

/// Whether [`load_handles()`] loads the handle of `phid`.
pub(crate) fn is_supported(phid: &Phid) -> bool {
    type_name(phid).is_some()
}

fn of_type(phids: &[Phid], code: &str) -> Vec<Phid> {
    let mut found: Vec<Phid> = phids
        .iter()
        .filter(|phid| phid.as_str().get(5..9) == Some(code))
        .cloned()
        .collect();
    found.sort();
    found.dedup();
    found
}

/// `HarbormasterBuild::getName()`: the name of its plan, if the viewer
/// can see it.
pub(crate) fn build_name(build: &Build, plans: &HashMap<Phid, Plan>) -> String {
    plans
        .get(&build.plan_phid)
        .map_or_else(|| "Build".to_owned(), harbormaster::plan_name)
}

/// `PhabricatorHandleQuery` as the viewer, for the objects Harbormaster
/// pages name. Objects of other types go to PHP.
pub(crate) async fn load_handles<S: WebState>(
    state: &S,
    scope: &Scope,
    phids: &[Phid],
) -> Result<HashMap<Phid, Handle>, PageError> {
    let mut phids: Vec<Phid> = phids.to_vec();
    phids.sort();
    phids.dedup();
    if phids.iter().any(|phid| type_name(phid).is_none()) {
        return Err(PageError::Fallback("a handle of an object not ported"));
    }
    let request = scope.request();
    let viewer = scope.viewer();
    let database = state.database();
    let config = state.config();
    let now = state.now();
    let mut found: HashMap<Phid, Handle> = HashMap::new();

    for user in phab_auth::user::load_users_by_phid(database, &of_type(&phids, "USER")).await? {
        let mut handle = Handle::plain(user.user_name.clone(), format!("/p/{}/", user.user_name));
        handle.closed = user.is_disabled;
        found.insert(user.phid, handle);
    }

    let projects = project::storage::load_by_phid(database, &of_type(&phids, "PROJ")).await?;
    let (projects, graph) =
        project::policy::visible_projects(database, config, &viewer, projects, now)
            .await
            .map_err(internal)?;
    for project in &projects {
        let mut handle = Handle::plain(project.display_name(graph.parent(project)), project.uri());
        handle.closed = project.is_archived();
        found.insert(project.phid.clone(), handle);
    }

    let revisions = differential::load_revisions(database, &of_type(&phids, "DREV")).await?;
    for revision in differential::visible_revisions(database, config, &viewer, revisions, now)
        .await
        .map_err(internal)?
    {
        let mut handle = Handle::plain(
            format!("D{}: {}", revision.id, revision.title),
            format!("/D{}", revision.id),
        );
        handle.closed = revision.is_closed();
        found.insert(revision.phid.clone(), handle);
    }

    for (id, phid) in diff_search::visible_diffs(&request, &of_type(&phids, "DIFF"))
        .await
        .map_err(method_error)?
    {
        found.insert(
            phid,
            Handle::plain(format!("Diff {id}"), format!("/differential/diff/{id}/")),
        );
    }

    let commits =
        commit::load_visible_by_phids(database, config, &viewer, &of_type(&phids, "CMIT"), now)
            .await
            .map_err(internal)?;
    let rewritten = commit::rewritten_to(database, commits.values()).await?;
    for (phid, commit) in commits {
        let (name, full_name) = commit.handle_names(rewritten.get(&phid).map(String::as_str));
        let mut handle = Handle::new(name, full_name, commit.uri());
        handle.closed = commit.is_unreachable;
        found.insert(phid, handle);
    }

    let repositories = repository::load(database, &of_type(&phids, "REPO")).await?;
    for repository in repository::visible(database, config, &viewer, repositories, now)
        .await
        .map_err(internal)?
    {
        let monogram = repository.monogram();
        found.insert(
            repository.phid.clone(),
            Handle::new(
                monogram.clone(),
                format!("{monogram} {}", repository.name),
                repository.uri(),
            ),
        );
    }

    let files = phab_app_files::load_by_phids(database, &of_type(&phids, "FILE")).await?;
    for file in crate::remarkup::visible_files(state, &viewer, files).await? {
        let monogram = file.monogram();
        found.insert(
            file.phid.clone(),
            Handle::new(
                monogram.clone(),
                format!("{monogram}: {}", file.name),
                format!("/{monogram}"),
            ),
        );
    }

    harbormaster_handles(state, scope, &phids, &mut found).await?;
    application_handles(state, &viewer, &of_type(&phids, "APPS"), &mut found).await?;

    missing_handles(database, &phids, &mut found).await?;
    Ok(found)
}

/// The handles of applications (`PhabricatorApplicationApplicationPHIDType`):
/// their name and URI, when the viewer may see them.
async fn application_handles<S: WebState>(
    state: &S,
    viewer: &Viewer,
    phids: &[Phid],
    found: &mut HashMap<Phid, Handle>,
) -> Result<(), PageError> {
    let known: Vec<(&Phid, &crate::system::catalog::Application)> = phids
        .iter()
        .filter_map(|phid| {
            let class = phid.as_str().strip_prefix("PHID-APPS-")?;
            Some((phid, crate::system::catalog::application(class)?))
        })
        .collect();
    let config = state.config();
    let policies = phab_policy::ApplicationPolicies::from_config(config);
    let subjects: Vec<phab_policy::PolicySubject> = known
        .iter()
        .map(|(_, application)| policies.subject(&application.class, config.allow_public()))
        .collect();
    let refs: Vec<&phab_policy::PolicySubject> = subjects.iter().collect();
    let facts = phab_policy::loader::load_facts(
        state.database(),
        config,
        viewer,
        &refs,
        &phab_policy::loader::NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let visible = phab_policy::filter(viewer, &refs, &[phab_policy::Capability::VIEW], &facts)
        .map_err(internal)?;
    for ((phid, application), visible) in known.into_iter().zip(visible) {
        let handle = match (visible, &application.base_uri) {
            (true, Some(uri)) => Handle::plain(application.name.clone(), uri.clone()),
            (true, None) => Handle {
                name: application.name.clone(),
                full_name: application.name.clone(),
                uri: None,
                closed: false,
            },
            (false, _) => Handle {
                name: "Restricted Application".to_owned(),
                full_name: "Restricted Application".to_owned(),
                uri: None,
                closed: false,
            },
        };
        found.insert(phid.clone(), handle);
    }
    // Classes which don't exist have no table to look in.
    for phid in phids {
        found.entry(phid.clone()).or_insert_with(|| Handle {
            name: "Unknown Object (Application)".to_owned(),
            full_name: "Unknown Object (Application)".to_owned(),
            uri: None,
            closed: false,
        });
    }
    Ok(())
}

/// The handles of the objects the viewer can't see: "Restricted" when
/// they exist, "Unknown Object" otherwise.
async fn missing_handles(
    database: &phab_db::Database,
    phids: &[Phid],
    found: &mut HashMap<Phid, Handle>,
) -> Result<(), PageError> {
    let missing: Vec<Phid> = phids
        .iter()
        .filter(|phid| !found.contains_key(*phid))
        .cloned()
        .collect();
    let existing = harbormaster::existing(database, &missing)
        .await?
        .unwrap_or_default();
    for phid in missing {
        let type_name = type_name(&phid).unwrap_or("Object");
        let name = if existing.contains(&phid) {
            format!("Restricted {type_name}")
        } else {
            format!("Unknown Object ({type_name})")
        };
        found.insert(
            phid,
            Handle {
                name: name.clone(),
                full_name: name,
                uri: None,
                closed: false,
            },
        );
    }
    Ok(())
}

/// The handles of Harbormaster's own objects: plans, builds and
/// buildables.
async fn harbormaster_handles<S: WebState>(
    state: &S,
    scope: &Scope,
    phids: &[Phid],
    found: &mut HashMap<Phid, Handle>,
) -> Result<(), PageError> {
    let request = scope.request();
    let plans = harbormaster::plans_by_phid(&request, &of_type(phids, "HMCP"))
        .await
        .map_err(method_error)?;
    for plan in plans.values() {
        found.insert(
            plan.phid.clone(),
            Handle::plain(
                format!("Plan {} {}", plan.id, harbormaster::plan_name(plan)),
                format!("/harbormaster/plan/{}/", plan.id),
            ),
        );
    }

    let builds = harbormaster::builds_by_phid(&request, &of_type(phids, "HMBD"))
        .await
        .map_err(method_error)?;
    let build_plans = harbormaster::plans_by_phid(
        &request,
        &builds
            .iter()
            .map(|build| build.plan_phid.clone())
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(method_error)?;
    for build in &builds {
        found.insert(
            build.phid.clone(),
            Handle::plain(
                format!("Build {}: {}", build.id, build_name(build, &build_plans)),
                format!("/harbormaster/build/{}/", build.id),
            ),
        );
    }

    let buildables = harbormaster::buildables_by_phid(&request, &of_type(phids, "HMBB"))
        .await
        .map_err(method_error)?;
    if !buildables.is_empty() {
        // `HarbormasterBuildablePHIDType` names the object built.
        let objects: Vec<Phid> = buildables
            .iter()
            .map(|buildable| buildable.object_phid.clone())
            .collect();
        let objects = Box::pin(load_handles(state, scope, &objects)).await?;
        for buildable in &buildables {
            let monogram = format!("B{}", buildable.id);
            let target = objects
                .get(&buildable.object_phid)
                .map_or("", |handle| handle.full_name.as_str());
            found.insert(
                buildable.phid.clone(),
                Handle::new(
                    monogram.clone(),
                    format!("{monogram}: {target}"),
                    format!("/{monogram}"),
                ),
            );
        }
    }

    Ok(())
}

/// The handle of `phid` among `handles`.
pub(crate) fn handle<'a>(handles: &'a HashMap<Phid, Handle>, phid: &Phid) -> &'a Handle {
    static UNKNOWN: std::sync::LazyLock<Handle> = std::sync::LazyLock::new(|| Handle {
        name: "Unknown Object".to_owned(),
        full_name: "Unknown Object".to_owned(),
        uri: None,
        closed: false,
    });
    handles.get(phid).unwrap_or(&UNKNOWN)
}

/// A command of the side column, which links to PHP's workflow.
pub(crate) struct Action {
    pub(crate) name: &'static str,
    pub(crate) icon: &'static str,
    pub(crate) uri: String,
    pub(crate) enabled: bool,
}

/// `PhabricatorApplicationTransactionView::shouldShowTransaction()` and
/// the titles of a Harbormaster object's transactions: the timeline, or a
/// fallback for a transaction it can't show. `title` gives the title of a
/// transaction from its author's link, `None` for hidden ones, or fails.
pub(crate) async fn timeline<S: WebState>(
    state: &S,
    scope: &Scope,
    table: TransactionTable,
    object: &Phid,
    title: impl Fn(&Transaction, &str) -> Result<Option<String>, PageError>,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let transactions = harbormaster::transactions(database, table, object).await?;
    let subjects: Vec<_> = transactions.iter().map(Transaction::subject).collect();
    let visible = differential::visible_subjects(
        database,
        state.config(),
        &scope.viewer(),
        &subjects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let shown: Vec<&Transaction> = transactions
        .iter()
        .zip(visible)
        .filter_map(|(transaction, visible)| visible.then_some(transaction))
        .collect();
    // The timeline pages its transactions, and comments are not ported.
    if shown.len() > 100 {
        return Err(PageError::Fallback("a long Harbormaster timeline"));
    }
    if shown
        .iter()
        .any(|transaction| transaction.comment_phid.is_some())
    {
        return Err(PageError::Fallback("a comment in a Harbormaster timeline"));
    }
    let authors: Vec<Phid> = shown
        .iter()
        .map(|transaction| transaction.author_phid.clone())
        .collect();
    if authors
        .iter()
        .any(|author| author.phid_type() != PhidType::USER)
    {
        return Err(PageError::Fallback("a transaction by an application"));
    }
    let handles = load_handles(state, scope, &authors).await?;
    let mut entries = Vec::new();
    for transaction in shown {
        let author = handle(&handles, &transaction.author_phid).short_link_html();
        if let Some(title_html) = title(transaction, &author)? {
            entries.push(Entry {
                title_html,
                date: time::format_utc(transaction.date_created),
                comment_html: None,
            });
        }
    }
    Ok(entries)
}

/// The commands a build takes (`HarbormasterBuildMessageTransaction`), in
/// the order the side columns list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Restart,
    Pause,
    Resume,
    Abort,
}

impl Command {
    pub(crate) const ALL: [Self; 4] = [Self::Restart, Self::Pause, Self::Resume, Self::Abort];

    /// `getHarbormasterBuildMessageType()`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Abort => "abort",
        }
    }

    /// `getHarbormasterBuildMessageName()`.
    pub(crate) fn build_name(self) -> &'static str {
        match self {
            Self::Restart => "Restart Build",
            Self::Pause => "Pause Build",
            Self::Resume => "Resume Build",
            Self::Abort => "Abort Build",
        }
    }

    /// `getHarbormasterBuildableMessageName()`.
    pub(crate) fn buildable_name(self) -> &'static str {
        match self {
            Self::Restart => "Restart Builds",
            Self::Pause => "Pause Builds",
            Self::Resume => "Resume Builds",
            Self::Abort => "Abort Builds",
        }
    }

    /// `getIcon()`.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Restart => "fa-repeat",
            Self::Pause => "fa-pause",
            Self::Resume => "fa-play",
            Self::Abort => "fa-exclamation-triangle",
        }
    }

    /// The command of a transaction type of builds (`message/restart`) or
    /// of a value of buildables' `harbormaster:buildable:command`.
    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.key() == key)
    }
}

/// A build with what deciding its commands needs.
pub(crate) struct BuildState<'a> {
    pub(crate) build: &'a Build,
    /// The status its unconsumed commands put it in.
    pub(crate) pending: Option<&'static str>,
    /// Its plan, if the viewer can see it, and whether they can edit it.
    pub(crate) plan: Option<(&'a Plan, bool)>,
}

impl BuildState<'_> {
    /// `getBuildPendingStatusObject()`.
    pub(crate) fn pending_status(&self) -> &str {
        self.pending.unwrap_or(&self.build.status)
    }

    /// `isAutobuild()`.
    fn is_autobuild(&self) -> bool {
        self.build.plan_auto_key.is_some()
    }

    /// `isComplete()`, of the stored status.
    fn is_complete(&self) -> bool {
        status::build_is_complete(&self.build.status)
    }

    /// `isPaused()`, of the stored status.
    fn is_paused(&self) -> bool {
        self.build.status == "paused"
    }

    /// `HarbormasterBuildMessageTransaction::canSendMessage()`.
    pub(crate) fn can_send(&self, command: Command) -> Result<bool, PageError> {
        let Some((plan, can_edit)) = self.plan else {
            return Ok(false);
        };
        if !plan::can_run_without_edit_capability(plan) && !can_edit {
            return Ok(false);
        }
        let pending = self.pending;
        let restarting = pending == Some("x-restarting");
        let aborting = pending == Some("x-aborting");
        let pausing = pending == Some("x-pausing");
        let resuming = pending == Some("x-resuming");
        // `newCanSendMessageAssertion()`, then `newCanApplyMessageAssertion()`.
        Ok(match command {
            Command::Restart => {
                if restarting || self.is_autobuild() {
                    return Ok(false);
                }
                match plan::behavior_option(plan, "restartable") {
                    "never" => false,
                    // PHP asks the transaction whether it failed, and
                    // fails.
                    "failed" => {
                        return Err(PageError::Fallback("a plan restartable if failed"));
                    }
                    _ => true,
                }
            }
            Command::Pause => {
                !(pausing
                    || restarting
                    || aborting
                    || self.is_autobuild()
                    || self.is_paused()
                    || self.is_complete())
            }
            Command::Resume => {
                !(resuming || restarting || aborting || self.is_autobuild())
                    && (self.is_paused() || pausing)
            }
            Command::Abort => !aborting && !self.is_autobuild() && !self.is_complete(),
        })
    }
}

/// The builds of `builds` with what deciding their commands needs.
pub(crate) async fn build_states<'a, S: WebState>(
    state: &S,
    scope: &Scope,
    builds: &'a [Build],
    plans: &'a HashMap<Phid, Plan>,
) -> Result<Vec<BuildState<'a>>, PageError> {
    let phids: Vec<Phid> = builds.iter().map(|build| build.phid.clone()).collect();
    let messages = harbormaster::messages(state.database(), &phids, true).await?;
    let plan_list: Vec<Plan> = plans.values().cloned().collect();
    let editable = harbormaster::can_edit_plans(&scope.request(), &plan_list)
        .await
        .map_err(method_error)?;
    let editable: HashSet<Phid> = plan_list
        .iter()
        .zip(editable)
        .filter(|(_, can_edit)| *can_edit)
        .map(|(plan, _)| plan.phid.clone())
        .collect();
    Ok(builds
        .iter()
        .map(|build| {
            let unconsumed: Vec<_> = messages
                .iter()
                .filter(|message| message.receiver_phid == build.phid)
                .cloned()
                .collect();
            BuildState {
                build,
                pending: harbormaster::pending_status(&unconsumed),
                plan: plans
                    .get(&build.plan_phid)
                    .map(|plan| (plan, editable.contains(&plan.phid))),
            }
        })
        .collect())
}

/// The title of a build command transaction (`message/restart`...).
pub(crate) fn command_title(command: Command, author: &str, object: &str) -> String {
    let verb = match command {
        Command::Restart => "restarted",
        Command::Pause => "paused",
        Command::Resume => "resumed",
        Command::Abort => "aborted",
    };
    format!("{author} {verb} this {object}.")
}
