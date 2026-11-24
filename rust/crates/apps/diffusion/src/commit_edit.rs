//! `diffusion.commit.edit` (`DiffusionCommitEditEngine` through
//! `PhabricatorEditEngine::buildConduitResponse()`): the transactions of
//! `PhabricatorAuditEditor` on an existing commit.
//!
//! An edit is planned first: every read and check, and the old and new
//! values of the transactions. Anything not ported is
//! [`MethodError::Unsupported`] there, before anything is written. Then
//! one MySQL transaction writes the rows PHP's editor does: the
//! transactions and the comment, the subscriber and project edges, the
//! audit requests (`updateAudits()`), the commit's audit status and its
//! `diffusion.commit.state` transaction (`applyFinalEffects()`), and the
//! `PhabricatorApplicationTransactionPublishWorker` task with the editor's
//! state, whose mail, feed story and search indexing PHP's daemons make.
//!
//! Herald doesn't run: `PhabricatorAuditEditor::shouldApplyHeraldRules()`
//! only applies the rules to the `audit:commit` transaction of the
//! publishing of a commit, which [`crate::import::publish::audit`] ports.
//!
//! Ported, on a Git commit the actor can see, named by its ID or its PHID:
//! a `comment` without remarkup effect; one audit action (`accept`,
//! `concern`, `resign`, `verify`) as the actor; auditors
//! (`auditors.add`, `.remove`, `.set`) which are users or projects;
//! subscribers and projects. A comment subscribes its author when nothing
//! else does (`applyImplicitCC()`). The mail and feed state is the
//! publisher's: none for commits the repository holds
//! (`shouldPublishRepositoryActivity()`).
//!
//! Left to PHP: other identifiers, other transactions (inline comments,
//! policies...), comments mentioning users or objects or attaching files,
//! comments on commits whose inline comments have a draft state, auditors
//! with an action, several transactions of a type, transactions without
//! effect, the errors PHP raises (an action PHP refuses, auditors who are
//! the author or unknown), packages, projects with watchers or which are
//! subprojects or milestones, and commits of other version control
//! systems.

mod audits;

use phab_app_differential::commit as differential_commit;
use phab_app_project::policy as project_policy;
use phab_conduit::edit::{self, Identifier, RawTransaction};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php, random};
use phab_db::editor::{self as db_editor, CONDUIT_SOURCE, TransactionRow};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, edges, rows, worker};
use serde_json::{Map, Value, json};

use self::audits::{Action, Audit, Operation};
use crate::import::commit::Commit;
use crate::import::repository::ImportRepository;

/// `PhabricatorRepositoryCommit::IMPORTED_MESSAGE | IMPORTED_CHANGE`
/// (`isCommitMostlyImported()`).
const MOSTLY_IMPORTED: i64 = 1 | 2;
/// `PhabricatorUnsubscribedFromObjectEdgeType`.
const UNSUBSCRIBED_FROM_OBJECT: i64 = 24;
/// `PhabricatorProjectProjectHasObjectEdgeType`.
const PROJECT_HAS_OBJECT: i64 = 42;
/// `PhabricatorProjectObjectHasWatcherEdgeType`.
const PROJECT_HAS_WATCHER: i64 = 47;
/// The configuration letting authors accept their own commits.
const CAN_AUTHOR_CLOSE: &str = "audit.can-author-close-audit";
/// A transaction without effect: PHP drops it (`setContinueOnNoEffect()`).
const NO_EFFECT: &str = "edits without effect";
/// The object references which mention nothing: projects, Herald rules,
/// Spaces, repositories and files are not `PhabricatorMentionableInterface`
/// (files attach, which [`check_comment()`] leaves to PHP).
const UNMENTIONABLE_PREFIXES: &[&str] = &["#", "H", "S", "r", "R", "F"];

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// A list transaction of the request: what it does and its PHIDs, each
/// once (`array_fuse()`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListEdit {
    operation: Operation,
    phids: Vec<String>,
}

/// One transaction of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    Comment(String),
    Action(Action),
    Auditors(ListEdit),
    Subscribers(ListEdit),
    Projects(ListEdit),
}

/// `isEmptyComment()`.
fn is_empty_comment(text: &str) -> bool {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
        .is_empty()
}

/// Whether `text` is a PHID of one of `types` (`PHID-USER-`, ...), with a
/// plain random part.
fn is_phid_of(text: &str, types: &[&str]) -> bool {
    types.iter().any(|prefix| {
        text.strip_prefix(prefix).is_some_and(|random| {
            !random.is_empty()
                && random
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    })
}

/// A list transaction (`auditors.add`, ...) whose value lists PHIDs of
/// `types`; names and datasource functions are left to PHP.
fn list_edit(operation: &str, value: &Value, types: &[&str]) -> Result<ListEdit, MethodError> {
    let operation = match operation {
        "add" => Operation::Add,
        "remove" => Operation::Remove,
        "set" => Operation::Set,
        _ => return Err(unsupported("transactions not ported")),
    };
    let not_phids = || unsupported("list values other than PHIDs");
    let Value::Array(items) = value else {
        return Err(not_phids());
    };
    let mut phids: Vec<String> = Vec::new();
    for item in items {
        let phid = item
            .as_str()
            .filter(|text| is_phid_of(text, types))
            .ok_or_else(not_phids)?;
        if !phids.iter().any(|known| known == phid) {
            phids.push(phid.to_owned());
        }
    }
    Ok(ListEdit { operation, phids })
}

/// The transactions of the request, as the edit types build them: one of
/// each, and one action.
fn changes(request: &ApiRequest<'_>, raw: &[RawTransaction]) -> Result<Vec<Change>, MethodError> {
    let families: Vec<&str> = raw
        .iter()
        .map(|xaction| {
            let kind = xaction.kind.as_str();
            match kind.split_once('.') {
                Some((list, _)) => list,
                None if Action::of_conduit_type(kind).is_some() => "action",
                None => kind,
            }
        })
        .collect();
    edit::check_unique(&families)?;
    let mut changes = Vec::new();
    for xaction in raw {
        let kind = xaction.kind.as_str();
        let change = if kind == "comment" {
            let text = edit::string(kind, &xaction.value)?;
            if is_empty_comment(&text) {
                return Err(unsupported("empty comments"));
            }
            Change::Comment(text)
        } else if let Some(action) = Action::of_conduit_type(kind) {
            if !edit::boolean(kind, &xaction.value, request.strictly_typed)? {
                return Err(unsupported("actions which are not true"));
            }
            Change::Action(action)
        } else {
            let value = &xaction.value;
            match kind.split_once('.') {
                Some(("auditors", operation)) => {
                    Change::Auditors(list_edit(operation, value, &["PHID-USER-", "PHID-PROJ-"])?)
                }
                Some(("subscribers", operation)) => {
                    Change::Subscribers(list_edit(operation, value, &["PHID-USER-", "PHID-PROJ-"])?)
                }
                Some(("projects", operation)) => {
                    Change::Projects(list_edit(operation, value, &["PHID-PROJ-"])?)
                }
                _ => return Err(unsupported("transactions not ported")),
            }
        };
        changes.push(change);
    }
    if changes.is_empty() {
        return Err(unsupported("empty transaction lists"));
    }
    // An auditor edit deletes requests the action's effect would still see
    // (`updateAudits()` keeps them attached).
    let has = |test: fn(&Change) -> bool| changes.iter().any(test);
    if has(|change| matches!(change, Change::Action(_)))
        && has(|change| matches!(change, Change::Auditors(_)))
    {
        return Err(unsupported("auditor edits with an action"));
    }
    Ok(changes)
}

/// The commit and what the edit reads about it.
struct Target {
    commit: Commit,
    repository: ImportRepository,
    /// `getEffectiveAuthorPHID()`.
    effective_author: Option<String>,
    /// The audit requests, as `needAuditRequests()` loads them.
    audits: Vec<Audit>,
    subscribers: Vec<String>,
    unsubscribers: Vec<String>,
    projects: Vec<String>,
    muted: Vec<String>,
    /// `shouldPublishRepositoryActivity()`: mail, feed and mentions.
    publishes: bool,
}

/// The destinations of the commit's edges of a type, newest first.
async fn destinations(
    database: &Database,
    phid: &str,
    edge_type: i64,
) -> Result<Vec<String>, MethodError> {
    Ok(
        edges::load_edges(database, &[Phid::new(phid)], &[edge_type], None)
            .await?
            .into_iter()
            .map(|edge| edge.dst.as_str().to_owned())
            .collect(),
    )
}

/// `needAuditRequests()`: `loadAllWhere()` without an order, as PHP runs
/// it, so the rows come in the same order.
async fn load_audits(
    connection: &mut MySqlConnection,
    database: &Database,
    commit: &str,
) -> Result<Vec<Audit>, MethodError> {
    let sql = format!(
        "SELECT * FROM {} WHERE commitPHID IN (?)",
        database.table("repository", "repository_auditrequest")
    );
    phab_db::sqlx::query(&sql)
        .bind(commit)
        .fetch_all(&mut *connection)
        .await?
        .iter()
        .map(|row| {
            Ok(Audit {
                id: rows::int(row, "id")?,
                auditor: rows::text(row, "auditorPHID")?,
                status: rows::text(row, "auditStatus")?,
            })
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// `getEffectiveAuthorPHID()`: the user of the author identity, or the
/// `authorPHID` of the commit's data.
async fn effective_author(
    database: &Database,
    commit: &Commit,
) -> Result<Option<String>, MethodError> {
    if let Some(identity) = &commit.author_identity_phid {
        let sql = format!(
            "SELECT currentEffectiveUserPHID FROM {} WHERE phid = ?",
            database.table("repository", "repository_identity")
        );
        let user = phab_db::sqlx::query(&sql)
            .bind(identity)
            .fetch_optional(database.pool())
            .await?
            .map(|row| rows::opt_text(&row, "currentEffectiveUserPHID"))
            .transpose()?
            .flatten();
        if user.is_some() {
            return Ok(user);
        }
    }
    let sql = format!(
        "SELECT commitDetails FROM {} WHERE commitID = ?",
        database.table("repository", "repository_commitdata")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(commit.id)
        .fetch_optional(database.pool())
        .await?
    else {
        return Err(unsupported("commits without their data"));
    };
    Ok(match rows::json(&row, "commitDetails")?.get("authorPHID") {
        Some(Value::String(phid)) => Some(phid.clone()),
        None | Some(Value::Null) => None,
        Some(_) => return Err(unsupported("unusual commit authors")),
    })
}

/// Projects whose watchers get mail (`getMailCC()`), and subprojects,
/// whose ancestors' watchers do, are left to PHP.
async fn check_projects(database: &Database, projects: &[String]) -> Result<(), MethodError> {
    if projects.is_empty() {
        return Ok(());
    }
    let phids: Vec<Phid> = projects.iter().map(Phid::new).collect();
    if !edges::load_edges(database, &phids, &[PROJECT_HAS_WATCHER], None)
        .await?
        .is_empty()
    {
        return Err(unsupported("projects with watchers"));
    }
    let sql = format!(
        "SELECT 1 FROM {} WHERE phid IN ({}) AND (parentProjectPHID IS NOT NULL OR \
         milestoneNumber IS NOT NULL) LIMIT 1",
        database.table("project", "project"),
        phab_db::placeholders(projects.len())
    );
    if phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        projects.iter().map(String::as_str),
    )
    .fetch_optional(database.pool())
    .await?
    .is_some()
    {
        return Err(unsupported("subprojects and milestones"));
    }
    Ok(())
}

/// `newObjectFromIdentifier()` with `CAN_VIEW`: the commit, its repository
/// and what the transactions read. Editing needs `CAN_EDIT`, which is
/// "All Users" for commits.
async fn load(request: &ApiRequest<'_>, identifier: Identifier) -> Result<Target, MethodError> {
    let context = request.context;
    let database = &context.database;
    let commit = match identifier {
        Identifier::Id(id) => Commit::load_by_id(database, id).await?,
        Identifier::Phid(phid) if phid.starts_with("PHID-CMIT-") => {
            Commit::load_by_phid(database, &phid).await?
        }
        Identifier::Phid(_) => None,
    }
    .ok_or_else(|| unsupported("unknown commits"))?;
    let visible = differential_commit::load_visible_by_phids(
        database,
        &context.config,
        &request.viewer,
        &[Phid::new(commit.phid.as_str())],
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if visible.is_empty() {
        return Err(unsupported("commits the viewer can't see"));
    }
    let repository = ImportRepository::load(database, commit.repository_id)
        .await?
        .ok_or_else(|| unsupported("commits without their repository"))?;
    if repository.vcs != "git" {
        return Err(unsupported("commits of other repositories"));
    }
    let publishes = repository.hold_reasons().is_empty()
        && commit.is_permanent()
        && commit.is_partially_imported(MOSTLY_IMPORTED);
    let audits = {
        let mut connection = database.pool().acquire().await?;
        load_audits(&mut connection, database, &commit.phid).await?
    };
    if audits
        .iter()
        .any(|audit| !is_phid_of(&audit.auditor, &["PHID-USER-", "PHID-PROJ-"]))
    {
        return Err(unsupported("auditors which are not users or projects"));
    }
    let phid = commit.phid.clone();
    let projects = destinations(database, &phid, edges::OBJECT_HAS_PROJECT).await?;
    check_projects(database, &projects).await?;
    Ok(Target {
        effective_author: effective_author(database, &commit).await?,
        audits,
        subscribers: destinations(database, &phid, edges::OBJECT_HAS_SUBSCRIBER).await?,
        unsubscribers: destinations(database, &phid, edges::OBJECT_HAS_UNSUBSCRIBER).await?,
        projects,
        muted: destinations(database, &phid, edges::MUTED_BY).await?,
        publishes,
        commit,
        repository,
    })
}

/// Whether every user of `phids` exists and every project is one the
/// actor can see (`PhabricatorObjectQuery`).
async fn all_valid(request: &ApiRequest<'_>, phids: &[String]) -> Result<bool, MethodError> {
    let context = request.context;
    let database = &context.database;
    let (users, projects): (Vec<&String>, Vec<&String>) = phids
        .iter()
        .partition(|phid| phid.starts_with("PHID-USER-"));
    if !users.is_empty() {
        let sql = format!(
            "SELECT phid FROM {} WHERE phid IN ({})",
            database.table("user", "user"),
            phab_db::placeholders(users.len())
        );
        let found = phab_db::bind_all(
            phab_db::sqlx::query(&sql),
            users.iter().map(|phid| phid.as_str()),
        )
        .fetch_all(database.pool())
        .await?
        .len();
        if found != users.len() {
            return Ok(false);
        }
    }
    if projects.is_empty() {
        return Ok(true);
    }
    let wanted: Vec<Phid> = projects
        .iter()
        .map(|phid| Phid::new(phid.as_str()))
        .collect();
    let loaded = phab_app_project::storage::load_by_phid(database, &wanted).await?;
    let (visible, _) = project_policy::visible_projects(
        database,
        &context.config,
        &request.viewer,
        loaded,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    Ok(visible.len() == projects.len())
}

/// `needAuditAuthority()`: the actor, and the projects among the auditors
/// the actor is a member of and can see; only themselves on their own
/// commits.
async fn authority(
    request: &ApiRequest<'_>,
    target: &Target,
    actor: &str,
) -> Result<Vec<String>, MethodError> {
    let mut authority = vec![actor.to_owned()];
    if target.commit.author_phid.as_deref() == Some(actor) {
        return Ok(authority);
    }
    let projects: Vec<Phid> = target
        .audits
        .iter()
        .filter(|audit| audit.auditor.starts_with("PHID-PROJ-"))
        .map(|audit| Phid::new(audit.auditor.as_str()))
        .collect();
    if projects.is_empty() {
        return Ok(authority);
    }
    let context = request.context;
    let database = &context.database;
    let member_of: Vec<Phid> = edges::load_edges(
        database,
        &projects,
        &[edges::PROJECT_MATERIALIZED_MEMBER],
        Some(&[Phid::new(actor)]),
    )
    .await?
    .into_iter()
    .map(|edge| edge.src)
    .collect();
    if member_of.is_empty() {
        return Ok(authority);
    }
    let loaded = phab_app_project::storage::load_by_phid(database, &member_of).await?;
    let (visible, _) = project_policy::visible_projects(
        database,
        &context.config,
        &request.viewer,
        loaded,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    authority.extend(
        visible
            .into_iter()
            .map(|project| project.phid.into_string()),
    );
    Ok(authority)
}

/// A comment Rust writes as PHP does: without remarkup effect (mentions,
/// mentioned objects, attached files), on a commit whose inline comments
/// have no draft state (`newInlineStateTransaction()`).
async fn check_comment(
    request: &ApiRequest<'_>,
    target: &Target,
    text: &str,
) -> Result<(), MethodError> {
    let context = request.context;
    let engine = phab_remarkup::phabricator::engine(phab_app_differential::markup::engine_config(
        &context.config,
    ));
    let prepared = engine.prepare(text.as_bytes(), phab_remarkup::Mode::Html);
    if !prepared.mentioned_usernames().is_empty() {
        return Err(unsupported("comments mentioning users"));
    }
    if !prepared.file_attach_intent_ids().is_empty() {
        return Err(unsupported("comments attaching files"));
    }
    if prepared
        .object_references()
        .iter()
        .any(|(prefix, _)| !UNMENTIONABLE_PREFIXES.contains(prefix))
    {
        return Err(unsupported("comments mentioning objects"));
    }
    let database = &context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE commitPHID = ? AND fixedState IN ('draft', 'undraft') LIMIT 1",
        database.table("audit", "audit_transaction_comment")
    );
    if phab_db::sqlx::query(&sql)
        .bind(&target.commit.phid)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(unsupported("inline comments with a draft state"));
    }
    Ok(())
}

/// `applyImplicitCC()`: a comment subscribes its author, unless they are
/// the commit's author (`isAutomaticallySubscribed()`), subscribed,
/// unsubscribed, or a bot.
async fn implies_subscription(
    database: &Database,
    target: &Target,
    actor: &str,
) -> Result<bool, MethodError> {
    if target.commit.author_phid.as_deref() == Some(actor)
        || !actor.starts_with("PHID-USER-")
        || target.subscribers.iter().any(|phid| phid == actor)
        || target.unsubscribers.iter().any(|phid| phid == actor)
    {
        return Ok(false);
    }
    let sql = format!(
        "SELECT isSystemAgent FROM {} WHERE phid = ?",
        database.table("user", "user")
    );
    let bot = phab_db::sqlx::query(&sql)
        .bind(actor)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::flag(&row, "isSystemAgent"))
        .transpose()?;
    Ok(bot == Some(false))
}

/// The items of `from` which are not in `other`, in order.
fn minus(from: &[String], other: &[String]) -> Vec<String> {
    from.iter()
        .filter(|item| !other.contains(item))
        .cloned()
        .collect()
}

/// `getPHIDList()` and `getEdgeTransactionNewValue()`: the old PHIDs a set
/// keeps, the set's, the added ones (with the implicit CC's actor last,
/// `mergePHIDOrEdgeTransactions()`), less the removed ones.
fn new_list(old: &[String], edit: Option<&ListEdit>, implicit: Option<&str>) -> Vec<String> {
    let set = edit
        .filter(|edit| edit.operation == Operation::Set)
        .map(|edit| &edit.phids);
    let mut result: Vec<String> = Vec::new();
    let mut put = |phid: &str| {
        if !result.iter().any(|known| known == phid) {
            result.push(phid.to_owned());
        }
    };
    for phid in old {
        if set.is_none_or(|set| set.contains(phid)) {
            put(phid);
        }
    }
    if let Some(edit) = edit
        && edit.operation != Operation::Remove
    {
        edit.phids.iter().for_each(|phid| put(phid));
    }
    if let Some(actor) = implicit {
        put(actor);
    }
    if let Some(edit) = edit
        && edit.operation == Operation::Remove
    {
        result.retain(|phid| !edit.phids.contains(phid));
    }
    result
}

/// A transaction of the edit, once its values are known.
#[derive(Debug, Clone)]
enum Planned {
    /// `core:subscribers`: the subscribers before and after.
    Subscribers {
        phid: String,
        old: Vec<String>,
        new: Vec<String>,
    },
    /// `core:edge` of the "has project" edge: the projects removed and
    /// added.
    Projects {
        phid: String,
        removed: Vec<String>,
        added: Vec<String>,
    },
    /// `diffusion.commit.auditors`: each auditor's status, before and after.
    Auditors {
        phid: String,
        old: Vec<(String, String)>,
        new: Vec<(String, String)>,
    },
    /// An audit action.
    Action { phid: String, action: Action },
    /// `core:comment`.
    Comment {
        phid: String,
        comment_phid: String,
        text: String,
    },
}

impl Planned {
    fn phid(&self) -> &str {
        match self {
            Self::Subscribers { phid, .. }
            | Self::Projects { phid, .. }
            | Self::Auditors { phid, .. }
            | Self::Action { phid, .. }
            | Self::Comment { phid, .. } => phid,
        }
    }
}

/// The planned edit.
struct Plan {
    actor: String,
    group_id: String,
    /// The transactions, in the order PHP saves them.
    transactions: Vec<Planned>,
    /// The audit requests deleted, then written.
    deleted_audits: Vec<i64>,
    audit_writes: Vec<audits::Write>,
    /// The commit's status after the internal effects.
    internal_status: String,
    subscribers: Vec<String>,
    projects: Vec<String>,
    /// `buildOldRecipientLists()`.
    old_recipients: (Vec<String>, Vec<String>),
}

/// A new transaction PHID of commits.
fn transaction_phid() -> String {
    Phid::generate(PhidType::TRANSACTION, Some(PhidType::COMMIT)).into_string()
}

/// `getMailTo()`: the author, the auditors who haven't resigned and the
/// actor.
fn mail_to(target: &Target, audits: &[Audit], actor: &str) -> Vec<String> {
    let mut to: Vec<String> = target
        .commit
        .author_phid
        .iter()
        .filter(|phid| !phid.is_empty())
        .cloned()
        .collect();
    to.extend(
        audits
            .iter()
            .filter(|audit| !audit.is_resigned())
            .map(|audit| audit.auditor.clone()),
    );
    to.push(actor.to_owned());
    to
}

/// `buildOldRecipientLists()`: the mail recipients before the edit, when
/// it sends mail.
fn old_recipients(target: &Target, actor: &str) -> (Vec<String>, Vec<String>) {
    if !target.publishes {
        return (Vec::new(), Vec::new());
    }
    (
        mail_to(target, &target.audits, actor),
        target.subscribers.clone(),
    )
}

/// The `core:subscribers` transaction of the implicit CC (`implicit`, the
/// actor) and of the explicit subscriber edit, merged
/// (`combineTransactions()`), and the subscribers after it.
fn plan_subscription(
    target: &Target,
    edit: Option<&ListEdit>,
    implicit: Option<&str>,
) -> Result<(Planned, Vec<String>), MethodError> {
    let old = target.subscribers.clone();
    let new = new_list(&old, edit, implicit);
    let (added, removed) = (minus(&new, &old), minus(&old, &new));
    if added.is_empty() && removed.is_empty() {
        return Err(unsupported(NO_EFFECT));
    }
    // The new edges are the newest, the last numbered first.
    let mut after: Vec<String> = added.iter().rev().cloned().collect();
    after.extend(minus(&old, &removed));
    let planned = Planned::Subscribers {
        phid: transaction_phid(),
        old,
        new,
    };
    Ok((planned, after))
}

/// The "has project" `core:edge` transaction, and the projects after it.
/// Projects the actor can't see, subprojects, milestones and projects with
/// watchers are left to PHP.
async fn plan_projects(
    request: &ApiRequest<'_>,
    target: &Target,
    edit: &ListEdit,
) -> Result<(Planned, Vec<String>), MethodError> {
    let old = target.projects.clone();
    let new = new_list(&old, Some(edit), None);
    let (added, removed) = (minus(&new, &old), minus(&old, &new));
    if added.is_empty() && removed.is_empty() {
        return Err(unsupported(NO_EFFECT));
    }
    if !added.is_empty() {
        if !all_valid(request, &added).await? {
            return Err(unsupported("projects PHP refuses"));
        }
        check_projects(&request.context.database, &added).await?;
    }
    let mut after: Vec<String> = added.iter().rev().cloned().collect();
    after.extend(minus(&old, &removed));
    let planned = Planned::Projects {
        phid: transaction_phid(),
        removed,
        added,
    };
    Ok((planned, after))
}

/// The `diffusion.commit.auditors` transaction (`generateNewValue()`, the
/// checks of `validateTransactions()`), the requests it deletes and those
/// `updateAudits()` writes.
async fn plan_auditors(
    request: &ApiRequest<'_>,
    target: &Target,
    edit: &ListEdit,
    can_author_close: bool,
) -> Result<(Planned, Vec<i64>, Vec<audits::Write>), MethodError> {
    let old = audits::auditor_map(&target.audits);
    let new = audits::new_auditors(&old, edit.operation, &edit.phids);
    let added: Vec<String> = new
        .iter()
        .filter(|(phid, _)| !old.iter().any(|(known, _)| known == phid))
        .map(|(phid, _)| phid.clone())
        .collect();
    if !added.is_empty() && !all_valid(request, &added).await? {
        return Err(unsupported("auditors PHP doesn't know"));
    }
    let author = target.effective_author.as_deref();
    if !can_author_close && added.iter().any(|phid| author == Some(phid.as_str())) {
        return Err(unsupported("auditors PHP refuses"));
    }
    if !audits::auditors_differ(&old, &new) {
        return Err(unsupported(NO_EFFECT));
    }
    // applyExternalEffects(): the removed requests go, then updateAudits().
    let deleted = target
        .audits
        .iter()
        .filter(|audit| !new.iter().any(|(phid, _)| *phid == audit.auditor))
        .map(|audit| audit.id)
        .collect();
    let writes = audits::updates(&target.audits, &new);
    let planned = Planned::Auditors {
        phid: transaction_phid(),
        old,
        new,
    };
    Ok((planned, deleted, writes))
}

/// Plans the edit of `target` by `changes`.
async fn plan(
    request: &ApiRequest<'_>,
    target: &Target,
    changes: Vec<Change>,
    actor: String,
) -> Result<Plan, MethodError> {
    let database = &request.context.database;
    let can_author_close = request
        .context
        .config
        .value(CAN_AUTHOR_CLOSE)
        .is_some_and(php::is_truthy);
    let authority = authority(request, target, &actor).await?;
    let audit_actor = audits::Actor {
        phid: &actor,
        effective_author: target.effective_author.as_deref(),
        authority: &authority,
        can_author_close,
    };

    let commented = changes
        .iter()
        .any(|change| matches!(change, Change::Comment(_)));
    let implicit = commented && implies_subscription(database, target, &actor).await?;
    let explicit_subscribers = changes.iter().find_map(|change| match change {
        Change::Subscribers(edit) => Some(edit),
        _ => None,
    });

    let mut head: Vec<Planned> = Vec::new();
    let mut tail: Vec<Planned> = Vec::new();
    let mut subscribers = target.subscribers.clone();
    let mut projects = target.projects.clone();
    let mut deleted_audits = Vec::new();
    let mut audit_writes = Vec::new();
    let mut internal_status = target.commit.audit_status.clone();

    // The implicit CC is unshifted, and the explicit subscriber edit merges
    // into it (`combineTransactions()`).
    if implicit {
        let (planned, after) =
            plan_subscription(target, explicit_subscribers, Some(actor.as_str()))?;
        head.push(planned);
        subscribers = after;
    }

    for change in &changes {
        match change {
            Change::Comment(text) => {
                check_comment(request, target, text).await?;
                tail.push(Planned::Comment {
                    phid: transaction_phid(),
                    comment_phid: Phid::generate(PhidType::TRANSACTION_COMMENT, None).into_string(),
                    text: text.clone(),
                });
            }
            Change::Subscribers(edit) => {
                if edit.operation != Operation::Remove && !all_valid(request, &edit.phids).await? {
                    return Err(unsupported("subscribers PHP doesn't know"));
                }
                if !implicit {
                    let (planned, after) = plan_subscription(target, Some(edit), None)?;
                    head.push(planned);
                    subscribers = after;
                }
            }
            Change::Projects(edit) => {
                let (planned, after) = plan_projects(request, target, edit).await?;
                head.push(planned);
                projects = after;
            }
            Change::Auditors(edit) => {
                let (planned, deleted, writes) =
                    plan_auditors(request, target, edit, can_author_close).await?;
                head.push(planned);
                (deleted_audits, audit_writes) = (deleted, writes);
            }
            Change::Action(action) => {
                if !audits::is_allowed(*action, &audit_actor, &target.audits, &internal_status) {
                    return Err(unsupported("audit actions PHP refuses"));
                }
                if let Some(status) = action.internal_status() {
                    status.clone_into(&mut internal_status);
                }
                let map = audits::action_statuses(*action, &audit_actor, &target.audits);
                audit_writes = audits::updates(&target.audits, &map);
                head.push(Planned::Action {
                    phid: transaction_phid(),
                    action: *action,
                });
            }
        }
    }

    let old_recipients = old_recipients(target, &actor);
    // sortTransactions(): comments go last.
    head.extend(tail);
    Ok(Plan {
        actor,
        group_id: random::random_characters(32),
        transactions: head,
        deleted_audits,
        audit_writes,
        internal_status,
        subscribers,
        projects,
        old_recipients,
    })
}

/// A map of PHP's, `[]` when empty.
fn map_value(pairs: &[(String, String)]) -> Value {
    if pairs.is_empty() {
        return json!([]);
    }
    Value::Object(
        pairs
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect(),
    )
}

/// `getHeraldRuleMonograms()`: `<1>, <2>` is `["H1", "H2"]`; PHP unsets
/// what is not a rule, so a list with holes is a JSON object.
fn herald_monograms(header: &str) -> Value {
    let mut map = Map::new();
    let mut holes = false;
    for (index, item) in header
        .split([',', ' '])
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        let item = item.trim_matches(['<', '>']);
        if item.is_empty() || !item.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        holes |= index != map.len();
        map.insert(index.to_string(), json!(format!("H{item}")));
    }
    if holes {
        Value::Object(map)
    } else {
        Value::Array(map.into_iter().map(|(_, value)| value).collect())
    }
}

/// `applyOldRecipientLists()`: who the edit took off the recipient lists
/// still gets its mail, unless they took themselves off. Returns them.
fn add_old_recipients(
    (to, cc): (&mut Vec<String>, &mut Vec<String>),
    (old_to, old_cc): &(Vec<String>, Vec<String>),
    actor: &str,
) -> Vec<String> {
    let known: Vec<String> = to.iter().chain(cc.iter()).cloned().collect();
    let mut removed = Vec::new();
    for (list, old) in [(to, old_to), (cc, old_cc)] {
        for phid in old {
            if phid == actor || known.contains(phid) {
                continue;
            }
            list.push(phid.clone());
            removed.push(phid.clone());
        }
    }
    removed
}

/// The `state` of the publish task (`getWorkerState()`), with the audit
/// requests as the edit leaves them (`willPublish()` reloads them).
fn worker_state(target: &Target, plan: &Plan, audits: &[Audit], header: Option<&str>) -> Value {
    let commit = &target.commit;
    let actor = plan.actor.as_str();
    // getCustomWorkerState(): no patch (serialize(null), base64-encoded as
    // binary), no Herald to list the affected files, and the auditors
    // keyed by request ID.
    let auditor_phids = if audits.is_empty() {
        json!([])
    } else {
        Value::Object(
            audits
                .iter()
                .map(|audit| (audit.id.to_string(), json!(audit.auditor)))
                .collect(),
        )
    };
    let custom = json!({
        "rawPatch": "Tjs=",
        "affectedFiles": null,
        "auditorPHIDs": auditor_phids,
    });
    let mut state = json!({
        "parentMessageID": null,
        "isNewObject": false,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": header,
        "mailToPHIDs": [],
        "mailCCPHIDs": [],
        "feedNotifyPHIDs": [],
        "feedRelatedPHIDs": [],
        "feedShouldPublish": false,
        "mailShouldSend": false,
        "mustEncrypt": [],
        "mailStamps": [],
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": [],
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": custom,
        "custom.encoding": {"rawPatch": "binary"},
    });
    if !target.publishes {
        return state;
    }
    let mut to = mail_to(target, audits, actor);
    let mut cc = plan.subscribers.clone();
    let mut notify: Vec<String> = Vec::new();
    for phid in to.iter().chain(&cc) {
        if !notify.contains(phid) {
            notify.push(phid.clone());
        }
    }
    let removed = add_old_recipients((&mut to, &mut cc), &plan.old_recipients, actor);
    let mut related = vec![commit.phid.clone(), actor.to_owned()];
    related.extend(plan.projects.iter().cloned());
    let unexpandable: Vec<&str> = audits
        .iter()
        .filter(|audit| audit.is_resigned())
        .map(|audit| audit.auditor.as_str())
        .collect();
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    let stamps = vec![
        stamp("string", "application", json!("Diffusion")),
        stamp(
            "string",
            "monogram",
            json!(commit.monogram(&target.repository)),
        ),
        stamp("string", "phid", json!(commit.phid)),
        stamp("string", "object-type", json!("CMIT")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!("conduit")),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!([])),
        stamp("bool", "new", json!(false)),
        stamp("phid", "mention", json!([])),
        stamp("string", "herald", herald_monograms(header.unwrap_or(""))),
        stamp("phid", "removed", json!(removed)),
        stamp("phid", "tag", json!(plan.projects)),
        stamp("phid", "subscriber", json!(plan.subscribers)),
    ];
    state["mailToPHIDs"] = json!(to);
    state["mailCCPHIDs"] = json!(cc);
    state["feedNotifyPHIDs"] = json!(notify);
    state["feedRelatedPHIDs"] = json!(related);
    state["feedShouldPublish"] = json!(true);
    state["mailShouldSend"] = json!(true);
    state["mailStamps"] = json!(stamps);
    state["mailUnexpandablePHIDs"] = json!(unexpandable);
    state["mailMutedPHIDs"] = json!(target.muted);
    state
}

/// `loadXHeraldRulesHeader()`.
async fn saved_header(database: &Database, phid: &str) -> Result<Option<String>, MethodError> {
    let sql = format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::text(&row, "header"))
        .transpose()?)
}

/// The transaction row of a planned transaction without a comment.
async fn write_transaction(
    connection: &mut MySqlConnection,
    database: &Database,
    (plan, commit): (&Plan, &str),
    (phid, kind, old, new, metadata): (&str, &str, &Value, &Value, Map<String, Value>),
    now: i64,
) -> Result<(), MethodError> {
    let mut metadata = metadata;
    metadata.insert("core.groupID".to_owned(), json!(plan.group_id));
    let (old, new, metadata) = (
        php::json_encode(old),
        php::json_encode(new),
        php::json_encode(&Value::Object(metadata)),
    );
    let row = TransactionRow {
        phid,
        author: &plan.actor,
        object: commit,
        kind,
        old: &old,
        new: &new,
        metadata: &metadata,
        content_source: CONDUIT_SOURCE,
    };
    let table = database.table("audit", "audit_transaction");
    db_editor::insert_transaction(connection, &table, row, now).await?;
    Ok(())
}

/// The comment and its transaction (`PhabricatorAuditTransactionComment`
/// of no file or line).
async fn write_comment(
    connection: &mut MySqlConnection,
    database: &Database,
    (plan, commit): (&Plan, &str),
    (phid, comment_phid, text): (&str, &str, &str),
    now: i64,
) -> Result<(), MethodError> {
    let metadata = php::json_encode(&json!({"core.groupID": plan.group_id}));
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
         commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
         dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, ?, 1, 'core:comment', 'null', \
         'null', ?, ?, ?, ?)",
        database.table("audit", "audit_transaction")
    ))
    .bind(phid)
    .bind(&plan.actor)
    .bind(commit)
    .bind(&plan.actor)
    .bind(comment_phid)
    .bind(CONDUIT_SOURCE)
    .bind(&metadata)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
         commentVersion, content, contentSource, isDeleted, dateCreated, dateModified, \
         commitPHID, pathID, isNewFile, lineNumber, lineLength, fixedState, hasReplies, \
         replyToCommentPHID, legacyCommentID, attributes) VALUES (?, ?, ?, 'public', ?, 1, ?, \
         ?, 0, ?, ?, NULL, NULL, 0, 0, 0, NULL, 0, NULL, NULL, '[]')",
        database.table("audit", "audit_transaction_comment")
    ))
    .bind(comment_phid)
    .bind(phid)
    .bind(&plan.actor)
    .bind(&plan.actor)
    .bind(text)
    .bind(CONDUIT_SOURCE)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The inverse of an edge type the editor writes.
fn inverse_edge_type(edge_type: i64) -> i64 {
    match edge_type {
        edges::OBJECT_HAS_SUBSCRIBER => edges::SUBSCRIBED_TO_OBJECT,
        edges::OBJECT_HAS_UNSUBSCRIBER => UNSUBSCRIBED_FROM_OBJECT,
        _ => PROJECT_HAS_OBJECT,
    }
}

/// `PhabricatorSubscriptionsEditor::save()`: who the transaction
/// subscribes loses their "unsubscribed" edge, who it unsubscribes gets
/// one; `PhabricatorEdgeEditor` numbers the new edges of the commit.
async fn write_subscription_edges(
    connection: &mut MySqlConnection,
    database: &Database,
    commit: &str,
    (old, new): (&[String], &[String]),
    now: i64,
) -> Result<(), MethodError> {
    let changes = minus(new, old)
        .into_iter()
        .map(|phid| {
            (
                phid,
                edges::OBJECT_HAS_UNSUBSCRIBER,
                edges::OBJECT_HAS_SUBSCRIBER,
            )
        })
        .chain(minus(old, new).into_iter().map(|phid| {
            (
                phid,
                edges::OBJECT_HAS_SUBSCRIBER,
                edges::OBJECT_HAS_UNSUBSCRIBER,
            )
        }));
    for (seq, (phid, gone, added)) in (0_i64..).zip(changes) {
        for (src, edge_type, dst) in [
            (commit, gone, phid.as_str()),
            (&phid, inverse_edge_type(gone), commit),
        ] {
            db_editor::delete_edge(connection, database, (src, edge_type, dst)).await?;
        }
        db_editor::insert_edge(connection, database, (commit, added, &phid), seq, now).await?;
        db_editor::insert_edge(
            connection,
            database,
            (&phid, inverse_edge_type(added), commit),
            0,
            now,
        )
        .await?;
    }
    Ok(())
}

/// The audit requests `applyExternalEffects()` deletes and writes.
async fn write_audits(
    connection: &mut MySqlConnection,
    database: &Database,
    plan: &Plan,
    commit: &str,
) -> Result<(), MethodError> {
    let table = database.table("repository", "repository_auditrequest");
    for id in &plan.deleted_audits {
        phab_db::sqlx::query(&format!("DELETE FROM {table} WHERE id = ?"))
            .bind(id)
            .execute(&mut *connection)
            .await?;
    }
    for write in &plan.audit_writes {
        match write {
            audits::Write::Insert { auditor, status } => {
                phab_db::sqlx::query(&format!(
                    "INSERT INTO {table} (auditorPHID, commitPHID, auditStatus, auditReasons) \
                     VALUES (?, ?, ?, '[]')"
                ))
                .bind(auditor)
                .bind(commit)
                .bind(status)
                .execute(&mut *connection)
                .await?;
            }
            audits::Write::Update { id, status } => {
                phab_db::sqlx::query(&format!("UPDATE {table} SET auditStatus = ? WHERE id = ?"))
                    .bind(status)
                    .bind(id)
                    .execute(&mut *connection)
                    .await?;
            }
        }
    }
    Ok(())
}

/// `save()` of the commit, whose status is the only column which changes.
async fn save_status(
    connection: &mut MySqlConnection,
    database: &Database,
    id: i64,
    status: &str,
) -> Result<(), MethodError> {
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET auditStatus = ? WHERE id = ?",
        database.table("repository", "repository_commit")
    ))
    .bind(status)
    .bind(id)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The transaction rows, in order.
async fn write_transactions(
    connection: &mut MySqlConnection,
    database: &Database,
    (plan, commit): (&Plan, &str),
    now: i64,
) -> Result<(), MethodError> {
    for planned in &plan.transactions {
        let rows = (plan, commit);
        match planned {
            Planned::Subscribers { phid, old, new } => {
                let values = (&json!(old), &json!(new));
                let entry = (
                    phid.as_str(),
                    "core:subscribers",
                    values.0,
                    values.1,
                    Map::new(),
                );
                write_transaction(connection, database, rows, entry, now).await?;
            }
            Planned::Projects {
                phid,
                removed,
                added,
            } => {
                let mut metadata = Map::new();
                metadata.insert("edge:type".to_owned(), json!(edges::OBJECT_HAS_PROJECT));
                let values = (&json!(removed), &json!(added));
                let entry = (phid.as_str(), "core:edge", values.0, values.1, metadata);
                write_transaction(connection, database, rows, entry, now).await?;
            }
            Planned::Auditors { phid, old, new } => {
                let values = (&map_value(old), &map_value(new));
                let kind = "diffusion.commit.auditors";
                let entry = (phid.as_str(), kind, values.0, values.1, Map::new());
                write_transaction(connection, database, rows, entry, now).await?;
            }
            Planned::Action { phid, action } => {
                let entry = (
                    phid.as_str(),
                    action.transaction_type(),
                    &json!(false),
                    &json!(true),
                    Map::new(),
                );
                write_transaction(connection, database, rows, entry, now).await?;
            }
            Planned::Comment {
                phid,
                comment_phid,
                text,
            } => {
                let entry = (phid.as_str(), comment_phid.as_str(), text.as_str());
                write_comment(connection, database, rows, entry, now).await?;
            }
        }
    }
    Ok(())
}

/// `applyExternalEffects()`, in the order of the transactions.
async fn write_effects(
    connection: &mut MySqlConnection,
    database: &Database,
    (plan, commit): (&Plan, &str),
    now: i64,
) -> Result<(), MethodError> {
    for planned in &plan.transactions {
        match planned {
            Planned::Subscribers { old, new, .. } => {
                write_subscription_edges(connection, database, commit, (old, new), now).await?;
            }
            Planned::Projects { removed, added, .. } => {
                let types = (commit, edges::OBJECT_HAS_PROJECT, PROJECT_HAS_OBJECT);
                db_editor::save_edges(connection, database, types, (removed, added), now).await?;
            }
            Planned::Auditors { .. } | Planned::Action { .. } => {
                write_audits(connection, database, plan, commit).await?;
            }
            Planned::Comment { .. } => {}
        }
    }
    Ok(())
}

/// Writes the planned edit in one transaction, in PHP's order: the
/// transactions, their external effects, the final effects and the
/// publish task.
async fn write(request: &ApiRequest<'_>, target: &Target, plan: &Plan) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let commit = target.commit.phid.as_str();
    let header = saved_header(database, commit).await?;
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;

    // The commit's own save() after the internal effects.
    if plan.internal_status != target.commit.audit_status {
        save_status(
            connection,
            database,
            target.commit.id,
            &plan.internal_status,
        )
        .await?;
    }

    write_transactions(connection, database, (plan, commit), now).await?;
    write_effects(connection, database, (plan, commit), now).await?;

    // applyFinalEffects(): the commit's status from its reloaded requests,
    // and a transaction when it changes (but to "partially audited").
    let reloaded = load_audits(connection, database, commit).await?;
    let status = audits::commit_status(&reloaded, &plan.internal_status);
    if status != plan.internal_status {
        save_status(connection, database, target.commit.id, status).await?;
    }
    let old_status = target.commit.audit_status.as_str();
    if status != old_status && status != audits::STATUS_PARTIALLY_AUDITED {
        let row = TransactionRow {
            phid: &transaction_phid(),
            author: &plan.actor,
            object: commit,
            kind: "diffusion.commit.state",
            old: &php::json_encode(&json!(old_status)),
            new: &php::json_encode(&json!(status)),
            metadata: "[]",
            content_source: CONDUIT_SOURCE,
        };
        let table = database.table("audit", "audit_transaction");
        db_editor::insert_transaction(connection, &table, row, now).await?;
    }

    let data = php::json_encode(&json!({
        "objectPHID": commit,
        "actorPHID": plan.actor,
        "xactionPHIDs": plan.transactions.iter().map(Planned::phid).collect::<Vec<_>>(),
        "state": worker_state(target, plan, &reloaded, header.as_deref()),
    }));
    let task = worker::Task {
        class: "PhabricatorApplicationTransactionPublishWorker",
        data: &data,
        priority: worker::PRIORITY_ALERTS,
        object_phid: Some(commit),
    };
    worker::schedule_task(connection, database, task, now).await?;
    transaction.commit().await?;
    Ok(())
}

/// `diffusion.commit.edit`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommitEdit;

impl CommitEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        edit::require_writes(request)?;
        let actor = edit::actor(request)?;
        let raw = edit::raw_transactions(request)?;
        // Commits can't be created (a "No One" creation policy).
        let identifier = edit::identifier(request)?.ok_or_else(|| unsupported("creations"))?;
        let changes = changes(request, &raw)?;
        let target = load(request, identifier).await?;
        let plan = plan(request, &target, changes, actor).await?;
        write(request, &target, &plan).await?;
        Ok(json!({
            "object": {"id": target.commit.id, "phid": target.commit.phid},
            "transactions": plan
                .transactions
                .iter()
                .map(|planned| json!({"phid": planned.phid()}))
                .collect::<Vec<_>>(),
        }))
    }
}

impl Method for CommitEdit {
    fn name(&self) -> &'static str {
        "diffusion.commit.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn edit(operation: Operation, phids: &[&str]) -> ListEdit {
        ListEdit {
            operation,
            phids: strings(phids),
        }
    }

    #[test]
    fn merges_subscribers_like_php() {
        let old = strings(&["bob"]);
        let add = edit(Operation::Add, &["dave"]);
        assert_eq!(
            new_list(&old, Some(&add), Some("alice")),
            strings(&["bob", "dave", "alice"])
        );
        let set = edit(Operation::Set, &["dave"]);
        assert_eq!(
            new_list(&old, Some(&set), Some("alice")),
            strings(&["dave", "alice"])
        );
        let remove = edit(Operation::Remove, &["bob", "alice"]);
        assert_eq!(
            new_list(&old, Some(&remove), Some("alice")),
            Vec::<String>::new()
        );
        assert_eq!(
            new_list(&old, None, Some("alice")),
            strings(&["bob", "alice"])
        );
    }

    #[test]
    fn parses_lists_like_php() {
        let types = &["PHID-USER-", "PHID-PROJ-"];
        assert_eq!(
            list_edit("add", &json!(["PHID-USER-a", "PHID-USER-a"]), types).unwrap(),
            edit(Operation::Add, &["PHID-USER-a"])
        );
        assert!(list_edit("add", &json!(["alice"]), types).is_err());
        assert!(list_edit("add", &json!(["PHID-OPKG-a"]), types).is_err());
        assert!(list_edit("merge", &json!([]), types).is_err());
    }

    #[test]
    fn lists_monograms_like_php() {
        assert_eq!(herald_monograms(""), json!([]));
        assert_eq!(herald_monograms("none"), json!([]));
        assert_eq!(herald_monograms("<1>, <2>"), json!(["H1", "H2"]));
        assert_eq!(herald_monograms("none, <3>"), json!({"1": "H3"}));
    }

    #[test]
    fn encodes_maps_like_php() {
        assert_eq!(map_value(&[]), json!([]));
        assert_eq!(
            map_value(&[("PHID-USER-a".to_owned(), "requested".to_owned())]),
            json!({"PHID-USER-a": "requested"})
        );
    }
}
