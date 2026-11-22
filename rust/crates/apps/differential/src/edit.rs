//! `differential.revision.edit` (`DifferentialRevisionEditEngine` through
//! `PhabricatorEditEngine::buildConduitResponse()`): the transaction
//! editor.
//!
//! An edit is planned first: every read, check and Herald evaluation, and
//! the state the publish worker gets. Anything not ported is
//! [`MethodError::Unsupported`] at that point, before anything is written.
//! Then one MySQL transaction writes the rows PHP's editor would, and
//! queues PHP's `PhabricatorApplicationTransactionPublishWorker`, so mail,
//! feed stories and search indexing still come from PHP.
//!
//! Ported, on an existing revision the actor can see: a `comment`, whose
//! remarkup may mention users and reference revisions, commits
//! ([`commits`]) or files; one review or author action as the defaults, a
//! new `title`, `summary` or `testPlan`, or a reviewer edit
//! ([`reviewers`]); or a comment and one of those. With the review status
//! change they cause, when Herald's rules are ones
//! [`phab_app_herald::engine`] evaluates. A comment subscribes its author
//! when nothing else does (`applyImplicitCC()`).

mod commits;
mod dependencies;
mod files;
mod herald_actions;
mod herald_fields;
mod lists;
mod merge_checks;
mod monograms;
mod reverts;
mod reviewers;
mod stack;
mod subscribers;
mod tags;

use phab_app_herald::engine::{self, Adapter, Applied, Evaluation, Rule, Subject};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php, random};
use phab_db::{Database, edges, rows, worker};
use serde_json::{Value, json};

use crate::search::{self, Constraints, Order, Revisions};
use crate::storage::Revision;

/// The content source of a Conduit call (`PhabricatorConduitContentSource`).
const CONDUIT_SOURCE: &str = r#"{"source":"conduit","params":[]}"#;

/// `PhabricatorProjectObjectHasWatcherEdgeType`.
const PROJECT_HAS_WATCHER: i64 = 47;

/// `PhabricatorProjectProjectHasObjectEdgeType`.
const PROJECT_HAS_OBJECT: i64 = 42;

/// The Herald adapter of revisions (`HeraldDifferentialRevisionAdapter`),
/// for the fields and actions Rust implements.
struct RevisionAdapter;

impl Adapter for RevisionAdapter {
    fn has_field(&self, key: &str) -> bool {
        herald_fields::FIELDS.contains(&key)
    }

    fn has_action(&self, action: &str) -> bool {
        matches!(
            action,
            "email.must-encrypt"
                | "email.other"
                | "nothing"
                | "projects.add"
                | "projects.remove"
                | "subscribers.add"
                | "subscribers.remove"
                | "differential.reviewers.add"
                | "differential.reviewers.blocking"
                | "subscribers.self.add"
                | "subscribers.self.remove"
                | "differential.reviewers.self.add"
                | "differential.reviewers.self.blocking"
                | "email.self"
        )
    }
}

/// `isEmptyComment()`.
fn is_empty_comment(text: &str) -> bool {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
        .is_empty()
}

/// A review action: "Accept" or "Request Changes"
/// (`DifferentialRevisionReviewTransaction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Review {
    Accept,
    Reject,
    Resign,
}

impl Review {
    /// The reviewer status it sets.
    fn status(self) -> &'static str {
        match self {
            Self::Accept => "accepted",
            Self::Reject => "rejected",
            Self::Resign => "resigned",
        }
    }

    /// The transaction type.
    fn transaction_type(self) -> &'static str {
        match self {
            Self::Accept => "differential.revision.accept",
            Self::Reject => "differential.revision.reject",
            Self::Resign => "differential.revision.resign",
        }
    }
}

/// An action of the revision's author which sets its status: "Plan
/// Changes", "Abandon", "Request Review" and "Reclaim".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorAction {
    PlanChanges,
    Abandon,
    RequestReview,
    Reclaim,
}

impl AuthorAction {
    /// The status it sets: "Reclaim" sets "needs-review" on revisions
    /// which broadcast, which are the only ones ported.
    fn status(self) -> &'static str {
        match self {
            Self::PlanChanges => "changes-planned",
            Self::Abandon => "abandoned",
            Self::RequestReview | Self::Reclaim => "needs-review",
        }
    }

    /// The transaction type.
    fn transaction_type(self) -> &'static str {
        match self {
            Self::PlanChanges => "differential.revision.plan",
            Self::Abandon => "differential.revision.abandon",
            Self::RequestReview => "differential.revision.request",
            Self::Reclaim => "differential.revision.reclaim",
        }
    }
}

/// A remarkup field of revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextField {
    Summary,
    TestPlan,
}

impl TextField {
    /// The column.
    fn column(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::TestPlan => "testPlan",
        }
    }

    /// The transaction type.
    fn transaction_type(self) -> &'static str {
        match self {
            Self::Summary => "differential.revision.summary",
            Self::TestPlan => "differential.revision.testplan",
        }
    }
}

/// What the `transactions` parameter asks for.
/// The actions of a request, whose order PHP keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Review,
    Author,
    Title,
    Text,
    Reviewers,
    Subscribers,
    Projects,
    Stack,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Wanted {
    /// The actions, in the request's order: `sortTransactions()` only moves
    /// the comment last.
    order: Vec<Action>,
    /// A comment's text.
    comment: Option<String>,
    /// A review, for the reviewers the actor has authority over.
    review: Option<Review>,
    /// An action of the author.
    author_action: Option<AuthorAction>,
    /// A new title.
    title: Option<String>,
    /// A new summary or test plan.
    text: Option<(TextField, String)>,
    /// Reviewers added, removed or set.
    reviewers: Option<reviewers::Wanted>,
    /// Subscribers added, removed or set.
    subscribers: Option<lists::Wanted>,
    /// Tags added, removed or set.
    projects: Option<lists::Wanted>,
    /// Parent or child revisions added, removed or set, with the edge type.
    stack: Option<(i64, lists::Wanted)>,
}

/// The ported transactions of the `transactions` parameter: a comment, a
/// review, or both.
fn wanted(request: &ApiRequest<'_>) -> Result<Wanted, MethodError> {
    let Some(Value::Array(transactions)) = request.value("transactions") else {
        return Err(MethodError::Unsupported("unusual transaction lists"));
    };
    let mut wanted = Wanted::default();
    for transaction in transactions {
        match (
            transaction.get("type").and_then(Value::as_str),
            transaction.get("value"),
        ) {
            (Some("comment"), Some(Value::String(text))) if wanted.comment.is_none() => {
                if is_empty_comment(text) {
                    return Err(MethodError::Unsupported("empty comments"));
                }
                wanted.comment = Some(text.clone());
            }
            // Reviewing as the defaults; a list of reviewers is left to PHP.
            (Some(kind @ ("accept" | "reject" | "resign")), Some(Value::Bool(true)))
                if wanted.review.is_none() =>
            {
                wanted.review = Some(match kind {
                    "accept" => Review::Accept,
                    "reject" => Review::Reject,
                    _ => Review::Resign,
                });
                wanted.order.push(Action::Review);
            }
            (
                Some(kind @ ("plan-changes" | "abandon" | "request-review" | "reclaim")),
                Some(Value::Bool(true)),
            ) if wanted.author_action.is_none() => {
                wanted.author_action = Some(match kind {
                    "plan-changes" => AuthorAction::PlanChanges,
                    "abandon" => AuthorAction::Abandon,
                    "request-review" => AuthorAction::RequestReview,
                    _ => AuthorAction::Reclaim,
                });
                wanted.order.push(Action::Author);
            }
            (Some("title"), Some(Value::String(title))) if wanted.title.is_none() => {
                wanted.title = Some(title.clone());
                wanted.order.push(Action::Title);
            }
            (Some(kind @ ("summary" | "testPlan")), Some(Value::String(text)))
                if wanted.text.is_none() =>
            {
                let field = if kind == "summary" {
                    TextField::Summary
                } else {
                    TextField::TestPlan
                };
                wanted.text = Some((field, text.clone()));
                wanted.order.push(Action::Text);
            }
            (Some(kind), Some(value)) if kind.contains('.') => {
                wanted_list(&mut wanted, kind, value)?;
            }
            _ => return Err(MethodError::Unsupported("transactions not ported")),
        }
    }
    if wanted == Wanted::default() {
        return Err(MethodError::Unsupported("empty transaction lists"));
    }
    check_combination(&wanted)?;
    Ok(wanted)
}

/// Several actions, but those whose combination isn't ported: stack edits
/// (written with the links), "Request Review" (whose void comes later), and
/// reviews or author actions with a reviewer edit (both change reviewers).
fn check_combination(wanted: &Wanted) -> Result<(), MethodError> {
    if wanted.order.len() < 2 {
        return Ok(());
    }
    let has = |action| wanted.order.contains(&action);
    if has(Action::Stack)
        || wanted.author_action == Some(AuthorAction::RequestReview)
        || (has(Action::Reviewers) && (has(Action::Review) || has(Action::Author)))
    {
        return Err(MethodError::Unsupported("several actions"));
    }
    Ok(())
}

/// A list transaction (`reviewers.add`, `parents.set`, ...): one of each
/// list is ported.
fn wanted_list(wanted: &mut Wanted, kind: &str, value: &Value) -> Result<(), MethodError> {
    let unsupported = || MethodError::Unsupported("transactions not ported");
    let Some((list, "add" | "remove" | "set")) = kind.split_once('.') else {
        return Err(unsupported());
    };
    match list {
        "reviewers" if wanted.reviewers.is_none() => {
            wanted.reviewers = Some(reviewers::parse(kind, value)?);
            wanted.order.push(Action::Reviewers);
        }
        "subscribers" if wanted.subscribers.is_none() => {
            let types = &["PHID-USER-", "PHID-PROJ-"];
            wanted.subscribers = Some(lists::parse(kind, value, types)?);
            wanted.order.push(Action::Subscribers);
        }
        "projects" if wanted.projects.is_none() => {
            wanted.projects = Some(lists::parse(kind, value, &["PHID-PROJ-"])?);
            wanted.order.push(Action::Projects);
        }
        "parents" | "children" if wanted.stack.is_none() => {
            let edge_type = if list == "parents" {
                edges::REVISION_DEPENDS_ON_REVISION
            } else {
                edges::REVISION_DEPENDED_ON_BY_REVISION
            };
            wanted.stack = Some((edge_type, lists::parse(kind, value, &["PHID-DREV-"])?));
            wanted.order.push(Action::Stack);
        }
        _ => return Err(unsupported()),
    }
    Ok(())
}

/// `newObjectFromIdentifier()`: the revision, which the viewer must see.
async fn load_revision(request: &ApiRequest<'_>) -> Result<Revision, MethodError> {
    let id = match request.value("objectIdentifier") {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => {
            let digits = text.strip_prefix('D').unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                None
            } else {
                digits.parse().ok()
            }
        }
        _ => None,
    }
    .ok_or(MethodError::Unsupported(
        "creations and unusual identifiers",
    ))?;
    let query = Revisions {
        request,
        constraints: Constraints {
            ids: Some(vec![id]),
            ..Constraints::default()
        },
        order: Order::Updated,
        memory: std::sync::Mutex::default(),
    };
    phab_conduit::pager::execute(&query, None, phab_conduit::pager::Direction::Forward, 1)
        .await?
        .into_iter()
        .next()
        // Hidden revisions raise a policy exception; unknown ones an error.
        .ok_or(MethodError::Unsupported("revisions the viewer can't see"))
}

/// The revision's facts the edit reads beyond [`Revision`].
struct Facts {
    /// The `mention` mail stamp: the users the text newly mentions.
    mentioned: Value,
    should_broadcast: bool,
    reviewers: Vec<search::ReviewerRow>,
    projects: Vec<String>,
    subscribers: Vec<String>,
    muted: Vec<String>,
    /// `buildOldRecipientLists()`: the mail recipients before the edit.
    old_recipients: (Vec<Value>, Vec<Value>),
}

async fn destinations(
    database: &Database,
    phid: &Phid,
    edge_type: i64,
) -> Result<Vec<String>, MethodError> {
    Ok(
        edges::load_edges(database, std::slice::from_ref(phid), &[edge_type], None)
            .await?
            .into_iter()
            .map(|edge| edge.dst.as_str().to_owned())
            .collect(),
    )
}

impl Facts {
    async fn load(database: &Database, revision: &Revision) -> Result<Self, MethodError> {
        let sql = format!(
            "SELECT properties FROM {} WHERE id = ?",
            database.table("differential", "differential_revision")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(revision.id)
            .fetch_one(database.pool())
            .await?;
        // getShouldBroadcast(): the "draft.broadcast" property, true when
        // missing.
        let should_broadcast = match rows::json(&row, "properties")? {
            Value::Object(properties) => {
                properties.get("draft.broadcast").is_none_or(php::is_truthy)
            }
            _ => true,
        };
        let reviewers = search::load_reviewers(database, std::slice::from_ref(revision))
            .await?
            .remove(&revision.phid)
            .unwrap_or_default();
        let mut facts = Self {
            mentioned: json!([]),
            should_broadcast,
            reviewers,
            projects: destinations(database, &revision.phid, edges::OBJECT_HAS_PROJECT).await?,
            subscribers: destinations(database, &revision.phid, edges::OBJECT_HAS_SUBSCRIBER)
                .await?,
            muted: destinations(database, &revision.phid, edges::MUTED_BY).await?,
            old_recipients: (Vec::new(), Vec::new()),
        };
        facts.old_recipients = mail_recipients(revision, &facts);
        Ok(facts)
    }
}

/// Edits PHP's editor makes which Rust leaves to it: draft inline comments
/// to publish, build plans, and mail to project watchers.
async fn check_side_effects(
    request: &ApiRequest<'_>,
    revision: &Revision,
    facts: &Facts,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE revisionPHID = ? AND fixedState IN ('draft', 'undraft') LIMIT 1",
        database.table("differential", "differential_transaction_comment")
    );
    if phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(MethodError::Unsupported(
            "inline comments marked done in a draft",
        ));
    }
    if let Some(diff) = &revision.active_diff_phid {
        let sql = format!(
            "SELECT 1 FROM {} WHERE buildablePHID = ? LIMIT 1",
            database.table("harbormaster", "harbormaster_buildable")
        );
        if phab_db::sqlx::query(&sql)
            .bind(diff.as_str())
            .fetch_optional(database.pool())
            .await?
            .is_some()
        {
            return Err(MethodError::Unsupported("revisions with buildables"));
        }
    }
    check_projects(database, &facts.projects).await
}

/// Projects whose watchers would get mail, which is left to PHP.
async fn check_projects(database: &Database, projects: &[String]) -> Result<(), MethodError> {
    let projects: Vec<Phid> = projects.iter().cloned().map(Phid::new).collect();
    if !edges::load_edges(database, &projects, &[PROJECT_HAS_WATCHER], None)
        .await?
        .is_empty()
    {
        return Err(MethodError::Unsupported("projects with watchers"));
    }
    let sql = format!(
        "SELECT 1 FROM {} WHERE phid IN ({}) AND parentProjectPHID IS NOT NULL LIMIT 1",
        database.table("project", "project"),
        phab_db::placeholders(projects.len().max(1))
    );
    if !projects.is_empty()
        && phab_db::bind_all(
            phab_db::sqlx::query(&sql),
            projects.iter().map(Phid::as_str),
        )
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(MethodError::Unsupported(
            "subprojects, whose ancestors' watchers get mail",
        ));
    }
    Ok(())
}

/// The `core:subscribers` transaction of an edit: `applyImplicitCC()`'s
/// and `buildSubscribeTransaction()`'s, which `combineTransactions()`
/// merges.
struct Subscription {
    phid: String,
    /// The subscribers before and after, in PHP's order.
    old: Vec<String>,
    new: Vec<String>,
    /// Whether it comes first: an implicit CC is unshifted, a subscription
    /// of mentions alone is appended.
    first: bool,
    /// The `+` of its new value: the mentioned users, then the actor of an
    /// implicit CC, which an explicit edit merges into.
    added: Vec<String>,
}

/// `extractPHIDsFromMentions()` of a comment: the mentioned users, as the
/// people query returns them (newest first), with whether each is a bot,
/// an administrator, and an active account.
async fn mentioned_users(
    request: &ApiRequest<'_>,
    text: &str,
) -> Result<Vec<(String, bool, bool, bool)>, MethodError> {
    let database = &request.context.database;
    let engine =
        phab_remarkup::phabricator::engine(crate::markup::engine_config(&request.context.config));
    let usernames: Vec<String> = engine
        .prepare(text.as_bytes(), phab_remarkup::Mode::Html)
        .mentioned_usernames()
        .into_iter()
        .filter_map(|name| String::from_utf8(name).ok())
        .collect();
    if usernames.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT phid, isSystemAgent, isAdmin, isDisabled, isApproved FROM {} \
         WHERE userName IN ({}) ORDER BY id DESC",
        database.table("user", "user"),
        phab_db::placeholders(usernames.len())
    );
    phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        usernames.iter().map(String::as_str),
    )
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(|row| {
        Ok((
            rows::text(row, "phid")?,
            rows::flag(row, "isSystemAgent")?,
            rows::flag(row, "isAdmin")?,
            !rows::flag(row, "isDisabled")? && rows::flag(row, "isApproved")?,
        ))
    })
    .collect::<Result<_, phab_db::DbError>>()
    .map_err(Into::into)
}

/// The subscription a comment makes: its author (`applyImplicitCC()`) and
/// the users it mentions who can see the revision and aren't subscribed
/// (`buildSubscribeTransaction()`). Updates the subscribers of `facts` and
/// sets its `mention` stamp.
async fn plan_subscription(
    request: &ApiRequest<'_>,
    revision: &Revision,
    facts: &mut Facts,
    (wanted, old_text): (&Wanted, Option<&(TextField, String)>),
    actor: &str,
) -> Result<Option<Subscription>, MethodError> {
    let context = request.context;
    let database = &context.database;
    // shouldImplyCC(): only comments subscribe their author.
    let implicit =
        wanted.comment.is_some() && implies_subscription(database, revision, facts, actor).await?;
    // Only new mentions count: text a field had mentioned no one anew.
    let mut mentioned: Vec<(String, bool, bool, bool)> = Vec::new();
    for text in new_texts(wanted) {
        for user in mentioned_users(request, text).await? {
            if !mentioned.iter().any(|known| known.0 == user.0) {
                mentioned.push(user);
            }
        }
    }
    if let Some((_, old)) = old_text {
        let before = mentioned_users(request, old).await?;
        mentioned.retain(|user| !before.iter().any(|known| known.0 == user.0));
    }
    facts.mentioned = if mentioned.is_empty() {
        json!([])
    } else {
        // array_diff() keeps the PHID keys.
        Value::Object(
            mentioned
                .iter()
                .map(|(phid, ..)| (phid.clone(), json!(phid)))
                .collect(),
        )
    };
    let mut added: Vec<String> = Vec::new();
    for (phid, bot, admin, active) in &mentioned {
        let automatic = revision
            .author_phid
            .as_ref()
            .is_some_and(|author| author.as_str() == phid)
            || facts
                .reviewers
                .iter()
                .any(|reviewer| reviewer.reviewer == *phid && reviewer.status != "resigned");
        if *bot
            || facts.subscribers.contains(phid)
            || automatic
            || added.contains(phid)
            || (implicit && phid == actor)
        {
            continue;
        }
        if !active {
            return Err(MethodError::Unsupported("mentions of inactive users"));
        }
        let viewer = phab_policy::Viewer::user(Phid::new(phid.clone()), *admin);
        let visible = crate::storage::visible_revisions(
            database,
            &context.config,
            &viewer,
            vec![revision.clone()],
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        if !visible.is_empty() {
            added.push(phid.clone());
        }
    }
    // mergePHIDOrEdgeTransactions(): array_merge() puts the mentions
    // before the implicit CC they merge into.
    if implicit {
        added.push(actor.to_owned());
    }
    if added.is_empty() {
        return Ok(None);
    }
    // getLegacyRequiredCapabilities(): subscribing others needs CAN_EDIT,
    // or else CAN_INTERACT, which revisions don't have: PHP throws.
    if added.iter().any(|phid| phid != actor)
        && !crate::storage::can_edit(
            database,
            &context.config,
            &request.viewer,
            revision,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?
    {
        return Err(MethodError::Unsupported("subscriptions PHP refuses"));
    }
    let old = facts.subscribers.clone();
    // The new edges are the newest, the last numbered first.
    let mut after: Vec<String> = added.iter().rev().cloned().collect();
    after.extend(old.iter().cloned());
    facts.subscribers = after;
    let mut new = old.clone();
    new.extend(added.iter().cloned());
    Ok(Some(Subscription {
        phid: transaction_phid(),
        old,
        new,
        first: implicit,
        added,
    }))
}

/// The edges to other revisions an edit adds, mentions
/// (`expandRemarkupBlockTransactions()`) or dependencies: their `core:edge`
/// transaction, and the inverse edit of each other revision
/// (`applyInverseEdgeTransactions()`), a sub-editor which runs no Herald.
struct Link {
    edge_type: i64,
    /// The edge transaction.
    phid: String,
    /// The revisions the edges are removed from, which the viewer may not
    /// see, and those they are added to, in the order of the edit.
    removed: Vec<String>,
    added: Vec<String>,
    /// The other revisions the viewer can see, edges added or removed, in
    /// the order of `DifferentialRevisionQuery`.
    targets: Vec<Target>,
}

/// An object an edge goes to, or went to, and its inverse transaction.
struct Target {
    object: Linked,
    inverse_phid: String,
    added: bool,
}

/// The objects edges go to: revisions, with their facts, and commits.
enum Linked {
    Revision {
        revision: Box<Revision>,
        facts: Facts,
    },
    Commit(commits::Linked),
}

impl Target {
    fn phid(&self) -> &str {
        match &self.object {
            Linked::Revision { revision, .. } => revision.phid.as_str(),
            Linked::Commit(commit) => &commit.phid,
        }
    }
}

/// The revisions with these IDs the viewer can see, but `revision`.
async fn other_revisions(
    request: &ApiRequest<'_>,
    revision: &Revision,
    ids: Vec<i64>,
) -> Result<Vec<Revision>, MethodError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let constraints = Constraints {
        ids: Some(ids),
        ..Constraints::default()
    };
    visible_revisions(request, revision, constraints).await
}

/// The revisions matching `constraints` the viewer can see, but
/// `revision`, in the order of `DifferentialRevisionQuery`.
async fn visible_revisions(
    request: &ApiRequest<'_>,
    revision: &Revision,
    constraints: Constraints,
) -> Result<Vec<Revision>, MethodError> {
    let query = Revisions {
        request,
        constraints,
        order: Order::Updated,
        memory: std::sync::Mutex::default(),
    };
    Ok(phab_conduit::pager::execute(
        &query,
        None,
        phab_conduit::pager::Direction::Forward,
        usize::MAX,
    )
    .await?
    .into_iter()
    .filter(|found| found.phid != revision.phid)
    .collect())
}

/// The edges of `edge_type` from `revision` to `targets`, but those which
/// exist already (no effect). Targets whose inverse edit would do more than
/// PHP's ported editor does are left to PHP.
async fn plan_link(
    request: &ApiRequest<'_>,
    revision: &Revision,
    edge_type: i64,
    targets: Vec<Revision>,
) -> Result<Option<Link>, MethodError> {
    let database = &request.context.database;
    let old = destinations(database, &revision.phid, edge_type).await?;
    let mut planned = Vec::new();
    for target in targets {
        if old.iter().any(|phid| phid == target.phid.as_str()) {
            continue;
        }
        planned.push(plan_target(request, target, true).await?);
    }
    Ok(new_link(edge_type, planned))
}

/// The link adding edges to `targets`, if any.
fn new_link(edge_type: i64, targets: Vec<Target>) -> Option<Link> {
    if targets.is_empty() {
        return None;
    }
    Some(Link {
        edge_type,
        phid: transaction_phid(),
        removed: Vec::new(),
        added: targets
            .iter()
            .map(|target| target.phid().to_owned())
            .collect(),
        targets,
    })
}

/// The inverse edit of `target`, an edge to which is `added` or removed.
async fn plan_target(
    request: &ApiRequest<'_>,
    target: Revision,
    added: bool,
) -> Result<Target, MethodError> {
    let facts = Facts::load(&request.context.database, &target).await?;
    check_side_effects(request, &target, &facts).await?;
    if new_review_status(&target, sticky_accept(request))
        .is_some_and(|status| status != target.status)
    {
        return Err(MethodError::Unsupported(
            "linked revisions with a stale status",
        ));
    }
    Ok(Target {
        object: Linked::Revision {
            revision: Box::new(target),
            facts,
        },
        inverse_phid: transaction_phid(),
        added,
    })
}

/// The revisions `texts` reference, as `D123` or by URI, but the
/// `unmentionable` ones (dependencies and reverts), or the commits they
/// reference.
/// References to both are left to PHP, whose object query orders them.
async fn plan_object_mention(
    request: &ApiRequest<'_>,
    revision: &Revision,
    texts: &[&str],
    unmentionable: &[String],
) -> Result<Option<Link>, MethodError> {
    let engine =
        phab_remarkup::phabricator::engine(crate::markup::engine_config(&request.context.config));
    let references: Vec<(&'static str, Vec<u8>)> = texts
        .iter()
        .flat_map(|text| {
            engine
                .prepare(text.as_bytes(), phab_remarkup::Mode::Html)
                .object_references()
        })
        .collect();
    if references.is_empty() {
        return Ok(None);
    }
    let mut ids: Vec<i64> = Vec::new();
    let mut hashes: Vec<String> = Vec::new();
    for (prefix, id) in references {
        let Ok(id) = String::from_utf8(id) else {
            continue;
        };
        match (prefix, id.parse()) {
            ("D", Ok(id)) if !ids.contains(&id) => ids.push(id),
            ("", _) if !hashes.contains(&id) => hashes.push(id),
            // Projects, Herald rules, Spaces, repositories and files are not
            // mentionable (`PhabricatorMentionableInterface`); files attach
            // (`files::plan()`).
            ("D", Ok(_)) | ("" | "#" | "H" | "S" | "r" | "R" | "F", _) => {}
            _ => return Err(MethodError::Unsupported("references to other objects")),
        }
    }
    // Objects don't mention themselves.
    let mut found = other_revisions(request, revision, ids).await?;
    found.retain(|found| !unmentionable.iter().any(|phid| phid == found.phid.as_str()));
    let mut commits = commits::load(request, &hashes).await?;
    commits.retain(|commit| !unmentionable.contains(&commit.phid));
    if commits.is_empty() {
        return plan_link(request, revision, edges::OBJECT_MENTIONS_OBJECT, found).await;
    }
    if !found.is_empty() {
        return Err(MethodError::Unsupported(
            "references to revisions and commits",
        ));
    }
    commit_link(request, revision, edges::OBJECT_MENTIONS_OBJECT, commits).await
}

/// The edges of `edge_type` from `revision` to `commits`, but those which
/// exist already.
async fn commit_link(
    request: &ApiRequest<'_>,
    revision: &Revision,
    edge_type: i64,
    commits: Vec<commits::Linked>,
) -> Result<Option<Link>, MethodError> {
    let old = destinations(&request.context.database, &revision.phid, edge_type).await?;
    let targets = commits
        .into_iter()
        .filter(|commit| !old.contains(&commit.phid))
        .map(commit_target)
        .collect();
    Ok(new_link(edge_type, targets))
}

/// The inverse edit of a commit an edge is added to.
fn commit_target(commit: commits::Linked) -> Target {
    Target {
        object: Linked::Commit(commit),
        inverse_phid: Phid::generate(PhidType::TRANSACTION, Some(PhidType::COMMIT)).into_string(),
        added: true,
    }
}

/// `applyImplicitCC()`: whether commenting subscribes the actor, who is
/// neither the author, a reviewer, a subscriber, someone who unsubscribed,
/// nor a bot.
async fn implies_subscription(
    database: &Database,
    revision: &Revision,
    facts: &Facts,
    actor: &str,
) -> Result<bool, MethodError> {
    let automatic = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor)
        || facts
            .reviewers
            .iter()
            .any(|reviewer| reviewer.reviewer == actor && reviewer.status != "resigned");
    if automatic
        || !actor.starts_with("PHID-USER-")
        || facts.subscribers.iter().any(|phid| phid == actor)
    {
        return Ok(false);
    }
    if destinations(database, &revision.phid, edges::OBJECT_HAS_UNSUBSCRIBER)
        .await?
        .iter()
        .any(|phid| phid == actor)
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

/// `updateReviewStatus()`: the status the reviewers call for, if any.
fn new_review_status(revision: &Revision, sticky_accept: bool) -> Option<String> {
    let was_accepted = revision.status == "accepted";
    let was_revision = revision.status == "needs-revision";
    if !was_accepted && !was_revision && revision.status != "needs-review" {
        return None;
    }
    let active = revision.active_diff_phid.as_ref();
    let current = |reviewer: &crate::storage::Reviewer| {
        active.is_none_or(|diff| {
            reviewer
                .last_action_diff_phid
                .as_ref()
                .is_none_or(|action| action == diff)
        })
    };
    let (mut accepting, mut rejecting, mut rejecting_older, mut blocking) =
        (false, false, false, false);
    for reviewer in &revision.reviewers {
        match reviewer.status.as_str() {
            "rejected" => {
                if reviewer.voided_phid.is_none() && current(reviewer) {
                    rejecting = true;
                } else {
                    rejecting_older = true;
                }
            }
            "rejected-older" => rejecting_older = true,
            "blocking" => blocking = true,
            "accepted"
                if reviewer.phid.phid_type() == PhidType::USER
                    && reviewer.voided_phid.is_none()
                    && (current(reviewer) || sticky_accept) =>
            {
                accepting = true;
            }
            _ => {}
        }
    }
    let status = if accepting && !rejecting && !rejecting_older && !blocking {
        "accepted"
    } else if rejecting {
        "needs-revision"
    } else if was_accepted || was_revision {
        "needs-review"
    } else {
        return None;
    };
    Some(status.to_owned())
}

/// The reviewers a void affects: their row IDs and PHIDs.
type Voided = Vec<(i64, String)>;

/// The void transaction: its PHID, the reviewers it voids and the statuses
/// it must void.
type PlannedVoid = (String, Voided, Vec<&'static str>);

/// A reviewer row a review writes (`applyReviewerEffect()`).
struct ReviewerUpdate {
    reviewer: String,
    /// Whether the status changes, which records the actor.
    status_changed: bool,
    /// The `options` with `sticky` unset.
    options: String,
}

/// The planned actions of the edit.
struct Actions {
    /// Their order, the request's.
    order: Vec<Action>,
    /// The review, its transaction and its reviewers.
    review: Option<(Review, String, Vec<ReviewerUpdate>)>,
    /// The author's action and its transaction.
    author_action: Option<(AuthorAction, String)>,
    /// The `differential.revision.title` transaction, with the old title.
    title: Option<(String, String)>,
    /// The summary or test plan transaction: the field, its PHID, the old
    /// and the new text.
    text: Option<(TextField, String, String, String)>,
    /// The `differential.revision.reviewers` transaction.
    reviewer_edit: Option<reviewers::Planned>,
    /// The `core:subscribers` transaction of `subscribers.*`.
    subscriber_edit: Option<Subscription>,
    /// The `core:edge` transaction of `projects.*`.
    tag_edit: Option<tags::Planned>,
    /// The `differential.revision.void` transaction of "Request Review":
    /// its PHID, the reviewers it voids and the statuses it must void.
    void: Option<PlannedVoid>,
}

impl Actions {
    /// The transactions of the actions, in the order PHP applies them: an
    /// author action's void follows it.
    fn phids(&self) -> Vec<String> {
        let mut phids = Vec::new();
        for action in &self.order {
            match action {
                Action::Review => phids.extend(self.review.iter().map(|(_, phid, _)| phid)),
                Action::Author => {
                    phids.extend(self.author_action.iter().map(|(_, phid)| phid));
                    phids.extend(self.void.iter().map(|(phid, _, _)| phid));
                }
                Action::Title => phids.extend(self.title.iter().map(|(phid, _)| phid)),
                Action::Text => phids.extend(self.text.iter().map(|(_, phid, ..)| phid)),
                Action::Reviewers => phids.extend(self.reviewer_edit.iter().map(|edit| &edit.phid)),
                Action::Subscribers => {
                    phids.extend(self.subscriber_edit.iter().map(|edit| &edit.phid));
                }
                Action::Projects => phids.extend(self.tag_edit.iter().map(|edit| &edit.phid)),
                Action::Stack => {}
            }
        }
        phids.into_iter().cloned().collect()
    }
}

/// The planned edit.
struct Plan {
    /// The revision as the edit leaves it.
    revision: Revision,
    actor: String,
    /// The `core:subscribers` transaction `applyImplicitCC()` adds: its
    /// PHID and the subscribers before.
    subscription: Option<Subscription>,
    actions: Actions,
    /// The comment: its text, transaction and comment PHIDs.
    comment: Option<(String, String, String)>,
    /// The `differential.revision.status` transaction `updateReviewStatus()`
    /// adds, with the old status.
    status: Option<(String, String)>,
    /// The revisions the edit links to, dependencies then mentions, and
    /// the publish tasks of their targets.
    links: Vec<(Link, Vec<String>)>,
    /// The implicit `core:file` transaction of the files the texts
    /// reference.
    file_edit: Option<files::Planned>,
    /// The revisions whose merge conflicts to recheck, with those stacked
    /// above them: the linked revisions' sub-editors', then the edit's.
    merge_checks: Vec<String>,
    /// The Herald editor's transactions.
    herald: herald_actions::Edits,
    /// Every transaction, in the order PHP applies them.
    applied_phids: Vec<String>,
    group_id: String,
    rules: Vec<Rule>,
    transcript: engine::TranscriptRow,
    herald_header: String,
    worker_data: String,
}

/// `HeraldEngine::loadAndApplyRules()` and the effects, without writing.
async fn run_herald(
    request: &ApiRequest<'_>,
    revision: &Revision,
    facts: &Facts,
    (input, applied_phids): (&herald_fields::Input<'_>, &[String]),
) -> Result<HeraldOutcome, MethodError> {
    let database = &request.context.database;
    let unsupported = |error: engine::HeraldError| match error {
        engine::HeraldError::Unsupported(what) => MethodError::Unsupported(what),
        other => MethodError::internal(other),
    };
    let rules = engine::load_rules(database, "differential")
        .await
        .map_err(unsupported)?;
    engine::check_supported(&rules, &RevisionAdapter).map_err(unsupported)?;
    // Drafts forbid the mailable state (`setForbiddenAction()`), which the
    // email actions require: PHP records those rules as forbidden.
    let mails = |rule: &Rule| {
        rule.actions
            .iter()
            .any(|action| matches!(action.action.as_str(), "email.other" | "email.self"))
    };
    if !facts.should_broadcast && rules.iter().any(mails) {
        return Err(MethodError::Unsupported(
            "Herald mail rules on drafts, which forbid them",
        ));
    }
    let mut rules = rules;
    for rule in rules.iter_mut().filter(|rule| rule.rule_type == "personal") {
        herald_actions::check_author(request, (revision, facts), rule).await?;
    }
    let subject = Subject {
        phid: revision.phid.as_str().to_owned(),
        content_type: "differential".to_owned(),
        name: revision.title.clone(),
        applied_transaction_phids: Some(applied_phids.to_vec()),
    };
    let used = engine::fields_used(&rules);
    let values = herald_fields::values(database, revision, facts, input, &used).await?;
    let evaluation = engine::evaluate(&rules, &subject, &values);
    if let Some(what) = evaluation.unsupported {
        return Err(MethodError::Unsupported(what));
    }
    let mut applied = Vec::new();
    let mut must_encrypt = Vec::new();
    let mut queue = herald_actions::Queue::default();
    for effect in &evaluation.effects {
        let subject = (revision, facts);
        let state = (&mut queue, &mut must_encrypt);
        let Some(rule) = rules.iter().find(|rule| rule.phid == effect.rule_phid) else {
            return Err(MethodError::internal("an effect without its rule"));
        };
        let reasons = herald_actions::apply(request, subject, (rule, effect), input, state).await?;
        applied.push(reasons);
    }
    Ok(HeraldOutcome {
        rules,
        evaluation,
        applied,
        must_encrypt,
        queue,
    })
}

/// What Herald's evaluation and actions produce.
struct HeraldOutcome {
    rules: Vec<Rule>,
    evaluation: Evaluation,
    applied: Vec<Applied>,
    must_encrypt: Vec<String>,
    queue: herald_actions::Queue,
}

/// `getHeraldRuleMonograms()` of a header like "<1>, <2>".
fn rule_monograms(header: &str) -> Value {
    Value::Array(
        header
            .split([',', ' '])
            .filter(|part| !part.is_empty())
            .map(|part| part.trim_matches(['<', '>']))
            .filter(|item| !item.is_empty() && item.bytes().all(|byte| byte.is_ascii_digit()))
            .map(|item| json!(format!("H{item}")))
            .collect(),
    )
}

/// `getMailTo()` and `getMailCC()`: the author and the reviewers who
/// haven't resigned, and the subscribers; no one for drafts.
fn mail_recipients(revision: &Revision, facts: &Facts) -> (Vec<Value>, Vec<Value>) {
    if !facts.should_broadcast {
        return (Vec::new(), Vec::new());
    }
    let mut to = vec![json!(revision.author_phid.as_ref().map(Phid::as_str))];
    to.extend(
        facts
            .reviewers
            .iter()
            .filter(|reviewer| reviewer.status != "resigned")
            .map(|reviewer| json!(reviewer.reviewer)),
    );
    let cc = facts.subscribers.iter().map(|phid| json!(phid)).collect();
    (to, cc)
}

/// `applyOldRecipientLists()`: who the edit took off the recipient lists
/// still gets its mail, unless they took themselves off. Returns them.
fn add_old_recipients(
    (to, cc): (&mut Vec<Value>, &mut Vec<Value>),
    (old_to, old_cc): &(Vec<Value>, Vec<Value>),
    actor: &str,
) -> Vec<Value> {
    let known: Vec<Value> = to.iter().chain(cc.iter()).cloned().collect();
    let mut removed = Vec::new();
    for (list, old) in [(to, old_to), (cc, old_cc)] {
        for phid in old {
            if phid.as_str() == Some(actor) || known.contains(phid) {
                continue;
            }
            list.push(phid.clone());
            removed.push(phid.clone());
        }
    }
    removed
}

/// The `state` of the publish task (`getWorkerState()`).
fn worker_state(
    revision: &Revision,
    facts: &Facts,
    actor: &str,
    must_encrypt: &[String],
    herald_header: &str,
) -> Value {
    let active: Vec<&search::ReviewerRow> = facts
        .reviewers
        .iter()
        .filter(|reviewer| reviewer.status != "resigned")
        .collect();
    let reviewers: Vec<&str> = active
        .iter()
        .map(|reviewer| reviewer.reviewer.as_str())
        .collect();
    let blocking: Vec<&str> = active
        .iter()
        .filter(|reviewer| reviewer.status == "blocking")
        .map(|reviewer| reviewer.reviewer.as_str())
        .collect();
    let resigned: Vec<&str> = facts
        .reviewers
        .iter()
        .filter(|reviewer| reviewer.status == "resigned")
        .map(|reviewer| reviewer.reviewer.as_str())
        .collect();
    let author = revision.author_phid.as_ref().map(Phid::as_str);
    let (mut to, mut cc) = mail_recipients(revision, facts);
    // Feed notifications read the lists again, without the old recipients.
    let mut notify: Vec<Value> = Vec::new();
    for phid in to.iter().chain(&cc) {
        if !notify.contains(phid) {
            notify.push(phid.clone());
        }
    }
    let removed = add_old_recipients((&mut to, &mut cc), &facts.old_recipients, actor);
    let mut related = vec![json!(revision.phid.as_str()), json!(actor)];
    related.extend(facts.projects.iter().map(|phid| json!(phid)));
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    let stamps = vec![
        stamp("phid", "author", json!(author)),
        stamp("phid", "reviewer", json!(reviewers)),
        stamp("phid", "blocking-reviewer", json!(blocking)),
        stamp("phid", "resigned-reviewer", json!(resigned)),
        stamp(
            "phid",
            "revision-repository",
            json!(revision.repository_phid.as_ref().map(Phid::as_str)),
        ),
        stamp("phid", "revision-status", json!(revision.status)),
        stamp("string", "application", json!("Differential")),
        stamp("string", "monogram", json!(format!("D{}", revision.id))),
        stamp("string", "phid", json!(revision.phid.as_str())),
        stamp("string", "object-type", json!("DREV")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!("conduit")),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!(must_encrypt)),
        stamp("bool", "new", json!(false)),
        stamp("phid", "mention", facts.mentioned.clone()),
        stamp("string", "herald", rule_monograms(herald_header)),
        stamp("phid", "removed", json!(removed)),
        stamp("phid", "tag", json!(facts.projects)),
        stamp("phid", "subscriber", json!(facts.subscribers)),
    ];
    json!({
        "parentMessageID": null,
        "isNewObject": false,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": herald_header,
        "mailToPHIDs": to,
        "mailCCPHIDs": cc,
        "feedNotifyPHIDs": notify,
        "feedRelatedPHIDs": related,
        "feedShouldPublish": true,
        "mailShouldSend": true,
        "mustEncrypt": must_encrypt,
        "mailStamps": stamps,
        "mailUnexpandablePHIDs": resigned,
        "mailMutedPHIDs": facts.muted,
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": {
            "changedPriorToCommitURI": null,
            "firstBroadcast": false,
            "isDraftDemotion": null,
        },
        "custom.encoding": [],
    })
}

/// `loadXHeraldRulesHeader()`.
async fn saved_header(database: &Database, phid: &Phid) -> Result<Option<String>, MethodError> {
    let sql = format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid.as_str())
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::text(&row, "header"))
        .transpose()?)
}

/// `differential.sticky-accept`, true by default.
fn sticky_accept(request: &ApiRequest<'_>) -> bool {
    request
        .context
        .config
        .value("differential.sticky-accept")
        .is_none_or(php::is_truthy)
}

/// `loadReviewerAuthority()`: the reviewers the actor acts as, themselves
/// and the projects they belong to.
async fn reviewer_authority(
    database: &Database,
    facts: &Facts,
    actor: &str,
) -> Result<Vec<String>, MethodError> {
    let mut authority: Vec<String> = Vec::new();
    for reviewer in &facts.reviewers {
        let phid = Phid::new(reviewer.reviewer.clone());
        let member = phid.phid_type() == PhidType::PROJECT
            && !edges::load_edges(
                database,
                std::slice::from_ref(&phid),
                &[edges::PROJECT_MATERIALIZED_MEMBER],
                Some(&[Phid::new(actor)]),
            )
            .await?
            .is_empty();
        if reviewer.reviewer == actor || member {
            authority.push(reviewer.reviewer.clone());
        }
    }
    Ok(authority)
}

/// `DifferentialRevisionAcceptTransaction` and
/// `DifferentialRevisionRejectTransaction`: the reviewers a review acts as,
/// once PHP's validation passes; refusals are left to PHP, which explains
/// them. Updates the reviewers of `revision` and `facts`.
async fn plan_review(
    request: &ApiRequest<'_>,
    review: Review,
    revision: &mut Revision,
    facts: &mut Facts,
    actor: &str,
) -> Result<Vec<ReviewerUpdate>, MethodError> {
    let database = &request.context.database;
    let is_author = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor);
    let resigned = facts
        .reviewers
        .iter()
        .any(|reviewer| reviewer.reviewer == actor && reviewer.status == "resigned");
    let refused = match review {
        Review::Accept | Review::Reject => {
            revision.status == "draft" || !facts.should_broadcast || is_author
        }
        Review::Resign => resigned,
    };
    if revision.is_closed() || refused {
        return Err(MethodError::Unsupported("reviews PHP refuses"));
    }
    let mut authority = reviewer_authority(database, facts, actor).await?;
    // Resigning only affects the actor.
    if review == Review::Resign {
        authority.retain(|phid| phid == actor);
    }
    if !authority.iter().any(|phid| phid == actor) {
        return Err(MethodError::Unsupported(
            "reviews by people who don't review",
        ));
    }
    let active = revision.active_diff_phid.clone();
    let sticky = sticky_accept(request);
    let done = |reviewer: &crate::storage::Reviewer| match review {
        Review::Accept => reviewer.is_accepted(active.as_ref(), sticky),
        Review::Resign => false,
        // isRejected(): rejected, not voided, on the active diff.
        Review::Reject => {
            reviewer.status == "rejected"
                && reviewer.voided_phid.is_none()
                && active.as_ref().is_none_or(|diff| {
                    reviewer
                        .last_action_diff_phid
                        .as_ref()
                        .is_none_or(|action| action == diff)
                })
        }
    };
    let authorized = |reviewer: &&crate::storage::Reviewer| {
        authority.iter().any(|phid| phid == reviewer.phid.as_str())
    };
    // isViewerReviewerStatusFully(): the review would change nothing.
    if review != Review::Resign && revision.reviewers.iter().filter(authorized).all(done) {
        return Err(MethodError::Unsupported("reviews PHP refuses"));
    }
    // Accepting as the defaults leaves out the reviewers who accepted
    // already (getActionOptions()); requesting changes has no options.
    let authority: Vec<String> = facts
        .reviewers
        .iter()
        .map(|row| row.reviewer.clone())
        .filter(|phid| {
            revision
                .reviewers
                .iter()
                .find(|reviewer| reviewer.phid.as_str() == phid)
                .is_some_and(|reviewer| {
                    authorized(&reviewer) && (review != Review::Accept || !done(reviewer))
                })
        })
        .collect();
    reviewer_updates(database, review, revision, facts, &authority).await
}

/// The rows `applyReviewerEffect()` writes for `authority`, and the
/// reviewers of `revision` and `facts` once written.
async fn reviewer_updates(
    database: &Database,
    review: Review,
    revision: &mut Revision,
    facts: &mut Facts,
    authority: &[String],
) -> Result<Vec<ReviewerUpdate>, MethodError> {
    let active = revision.active_diff_phid.clone();
    let sql = format!(
        "SELECT reviewerPHID, options FROM {} WHERE revisionPHID = ? AND reviewerPHID IN ({})",
        database.table("differential", "differential_reviewer"),
        phab_db::placeholders(authority.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql).bind(revision.phid.as_str()),
        authority.iter().map(String::as_str),
    )
    .fetch_all(database.pool())
    .await?;
    let mut updates = Vec::new();
    for phid in authority {
        let row = found
            .iter()
            .find(|row| rows::text(row, "reviewerPHID").is_ok_and(|text| text == *phid))
            .ok_or_else(|| MethodError::internal("reviewer row vanished"))?;
        // setOption('sticky', null) on the stored options.
        let options = match rows::json(row, "options")? {
            Value::Object(mut options) => {
                options.insert("sticky".to_owned(), Value::Null);
                Value::Object(options)
            }
            Value::Array(list) if list.is_empty() => json!({"sticky": null}),
            _ => return Err(MethodError::Unsupported("unusual reviewer options")),
        };
        let reviewer = revision
            .reviewers
            .iter_mut()
            .find(|reviewer| reviewer.phid.as_str() == phid)
            .ok_or_else(|| MethodError::internal("reviewer vanished"))?;
        let status_changed = reviewer.status != review.status();
        review.status().clone_into(&mut reviewer.status);
        if active.is_some() {
            reviewer.last_action_diff_phid.clone_from(&active);
        }
        reviewer.voided_phid = None;
        if let Some(row) = facts.reviewers.iter_mut().find(|row| row.reviewer == *phid) {
            review.status().clone_into(&mut row.status);
        }
        updates.push(ReviewerUpdate {
            reviewer: phid.clone(),
            status_changed,
            options: php::json_encode(&options),
        });
    }
    Ok(updates)
}

/// The author's actions (`DifferentialRevisionPlanChangesTransaction`,
/// `...AbandonTransaction`, `...RequestReviewTransaction`,
/// `...ReclaimTransaction`): the new status, once PHP's validation passes.
/// Drafts promote or demote: those are left to PHP.
fn plan_author_action(
    request: &ApiRequest<'_>,
    action: AuthorAction,
    revision: &mut Revision,
    facts: &Facts,
    actor: &str,
) -> Result<(), MethodError> {
    let is_author = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor);
    let status = revision.status.as_str();
    let ported = match action {
        AuthorAction::PlanChanges => {
            is_author && !revision.is_closed() && !matches!(status, "draft" | "changes-planned")
        }
        AuthorAction::Abandon => {
            let allowed = is_author
                || request
                    .context
                    .config
                    .value("differential.always-allow-abandon")
                    .is_some_and(php::is_truthy);
            allowed && !revision.is_closed()
        }
        // Members of #secure-revision may request review of drafts.
        AuthorAction::RequestReview => {
            is_author
                && facts.should_broadcast
                && !revision.is_closed()
                && !matches!(status, "draft" | "needs-review")
        }
        AuthorAction::Reclaim => is_author && facts.should_broadcast && status == "abandoned",
    };
    if !ported {
        return Err(MethodError::Unsupported("author actions not ported"));
    }
    action.status().clone_into(&mut revision.status);
    Ok(())
}

/// The `differential.revision.void` transaction "Request Review" adds
/// (`expandTransaction()`): the reviewers whose accepts and rejections it
/// voids, and the statuses it must void. Voids `revision`'s reviewers.
async fn plan_void(
    request: &ApiRequest<'_>,
    revision: &mut Revision,
    old_status: &str,
    actor: &str,
) -> Result<Option<(Voided, Vec<&'static str>)>, MethodError> {
    let database = &request.context.database;
    // "Changes Planned" then "Request Review" keeps sticky accepts.
    let force: Vec<&'static str> = if old_status == "changes-planned" {
        Vec::new()
    } else {
        vec!["accepted"]
    };
    let sql = format!(
        "SELECT id, reviewerPHID, reviewerStatus, options FROM {} WHERE revisionPHID = ? AND \
         voidedPHID IS NULL AND reviewerStatus IN ('accepted', 'rejected') ORDER BY id",
        database.table("differential", "differential_reviewer")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .fetch_all(database.pool())
        .await?;
    let default = sticky_accept(request);
    let mut voided = Vec::new();
    for row in &found {
        let status = rows::text(row, "reviewerStatus")?;
        // getOption('sticky'), coalesced with the configuration.
        let sticky = match rows::json(row, "options")? {
            Value::Object(options) => match options.get("sticky") {
                None | Some(Value::Null) => default,
                Some(value) => php::is_truthy(value),
            },
            _ => default,
        };
        if status == "accepted" && !force.contains(&"accepted") && sticky {
            continue;
        }
        voided.push((rows::int(row, "id")?, rows::text(row, "reviewerPHID")?));
    }
    if voided.is_empty() {
        return Ok(None);
    }
    for reviewer in &mut revision.reviewers {
        if voided
            .iter()
            .any(|(_, phid)| phid == reviewer.phid.as_str())
        {
            reviewer.voided_phid = Some(Phid::new(actor));
        }
    }
    Ok(Some((voided, force)))
}

/// `DifferentialRevisionTitleTransaction`: the new title, which needs
/// `CAN_EDIT` (PHP raises a policy exception otherwise). Returns the old
/// title. An unchanged title has no effect, yet PHP still saves and
/// publishes the revision; that is left to it.
async fn plan_title(
    request: &ApiRequest<'_>,
    revision: &mut Revision,
    title: String,
) -> Result<String, MethodError> {
    if title == revision.title {
        return Err(MethodError::Unsupported("edits without effect"));
    }
    let context = request.context;
    let can_edit = crate::storage::can_edit(
        &context.database,
        &context.config,
        &request.viewer,
        revision,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if !can_edit {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    Ok(std::mem::replace(&mut revision.title, title))
}

/// The new remarkup texts of an edit: the field's, then the comment.
fn new_texts(wanted: &Wanted) -> Vec<&str> {
    wanted
        .text
        .iter()
        .map(|(_, text)| text.as_str())
        .chain(wanted.comment.as_deref())
        .collect()
}

/// A new summary or test plan, which needs `CAN_EDIT`: the old text. An
/// unchanged text is left to PHP, as an unchanged title is.
async fn plan_text(
    request: &ApiRequest<'_>,
    revision: &Revision,
    field: TextField,
    new: &str,
) -> Result<String, MethodError> {
    let context = request.context;
    let database = &context.database;
    let sql = format!(
        "SELECT {} AS text FROM {} WHERE id = ?",
        field.column(),
        database.table("differential", "differential_revision")
    );
    let old = rows::text(
        &phab_db::sqlx::query(&sql)
            .bind(revision.id)
            .fetch_one(database.pool())
            .await?,
        "text",
    )?;
    if old == new {
        return Err(MethodError::Unsupported("edits without effect"));
    }
    let can_edit = crate::storage::can_edit(
        database,
        &context.config,
        &request.viewer,
        revision,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if !can_edit {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    Ok(old)
}

/// A new transaction PHID of revisions.
fn transaction_phid() -> String {
    Phid::generate(PhidType::TRANSACTION, Some(PhidType::REVISION)).into_string()
}

/// What Herald and the publish task need of the planned edit.
struct Publication {
    must_encrypt: Vec<String>,
    /// The Herald editor's transactions.
    herald: herald_actions::Edits,
    rules: Vec<Rule>,
    transcript: engine::TranscriptRow,
    herald_header: String,
    worker_data: String,
}

/// What Herald's fields read of the edit: the actor, the summary or test
/// plan set, and whether the edit unsubscribes anyone.
fn herald_input<'a>(
    actor: &'a str,
    actions: &'a Actions,
    subscription: Option<&Subscription>,
) -> herald_fields::Input<'a> {
    let removes = |edit: &Subscription| !lists::minus(&edit.old, &edit.new).is_empty();
    let subscribed = subscription
        .into_iter()
        .chain(actions.subscriber_edit.as_ref())
        .flat_map(|edit| lists::minus(&edit.new, &edit.old))
        .collect();
    herald_fields::Input {
        subscribed,
        tagged: actions
            .tag_edit
            .as_ref()
            .map(|edit| edit.added.clone())
            .unwrap_or_default(),
        actor,
        text: actions
            .text
            .as_ref()
            .map(|(field, _, _, new)| (*field, new.as_str())),
        removes_subscribers: subscription.is_some_and(removes)
            || actions.subscriber_edit.as_ref().is_some_and(removes),
    }
}

/// Runs Herald on the edited revision, plans what its actions queue, and
/// builds the publish task, which lists the Herald editor's transaction
/// too and sees its effect.
async fn publication(
    request: &ApiRequest<'_>,
    revision: &mut Revision,
    facts: &mut Facts,
    input: &herald_fields::Input<'_>,
    applied_phids: &[String],
) -> Result<Publication, MethodError> {
    let database = &request.context.database;
    let outcome = run_herald(request, revision, facts, (input, applied_phids)).await?;
    let subject = Subject {
        phid: revision.phid.as_str().to_owned(),
        content_type: "differential".to_owned(),
        name: revision.title.clone(),
        applied_transaction_phids: Some(applied_phids.to_vec()),
    };
    let transcript = outcome.evaluation.transcript(&subject, &outcome.applied);
    let herald_header = engine::combine_headers(
        saved_header(database, &revision.phid).await?.as_deref(),
        &engine::rules_header(&outcome.applied),
    );
    let herald = herald_actions::edits(
        outcome.queue,
        (revision, facts),
        (&input.subscribed, &input.tagged),
        sticky_accept(request),
    )?;
    let mut published: Vec<String> = applied_phids.to_vec();
    published.extend(herald.phids());
    let mut state = worker_state(
        revision,
        facts,
        input.actor,
        &outcome.must_encrypt,
        &herald_header,
    );
    state["heraldEmailPHIDs"] = json!(herald.email);
    state["heraldForcedEmailPHIDs"] = json!(herald.forced_email);
    let worker_data = php::json_encode(&json!({
        "objectPHID": revision.phid.as_str(),
        "actorPHID": input.actor,
        "xactionPHIDs": published,
        "state": state,
    }));
    Ok(Publication {
        must_encrypt: outcome.must_encrypt,
        herald,
        rules: outcome.rules,
        transcript,
        herald_header,
        worker_data,
    })
}

/// The publish task of a linked revision, with the Herald outcome
/// `mustEncrypt` its parent editor propagates.
async fn link_task(
    database: &Database,
    target: &Target,
    actor: &str,
    must_encrypt: &[String],
) -> Result<String, MethodError> {
    // Sub-editors run no Herald: they load the saved header, null when the
    // object has none.
    let saved = saved_header(database, &Phid::new(target.phid())).await?;
    let header = saved.as_deref().unwrap_or_default();
    let mut state = match &target.object {
        Linked::Revision { revision, facts } => {
            worker_state(revision, facts, actor, must_encrypt, header)
        }
        Linked::Commit(commit) => commits::worker_state(commit, actor, must_encrypt, header),
    };
    state["heraldHeader"] = json!(saved);
    Ok(php::json_encode(&json!({
        "objectPHID": target.phid(),
        "actorPHID": actor,
        "xactionPHIDs": [target.inverse_phid],
        "state": state,
    })))
}

/// The links with the publish tasks of their targets.
async fn link_tasks(
    database: &Database,
    links: Vec<Link>,
    actor: &str,
    must_encrypt: &[String],
) -> Result<Vec<(Link, Vec<String>)>, MethodError> {
    let mut tasks = Vec::new();
    for link in links {
        let mut data = Vec::new();
        for target in &link.targets {
            data.push(link_task(database, target, actor, must_encrypt).await?);
        }
        tasks.push((link, data));
    }
    Ok(tasks)
}

/// An author action, with the void "Request Review" adds.
async fn plan_author(
    request: &ApiRequest<'_>,
    action: AuthorAction,
    revision: &mut Revision,
    facts: &Facts,
    actor: &str,
) -> Result<(Option<(AuthorAction, String)>, Option<PlannedVoid>), MethodError> {
    let old_status = revision.status.clone();
    plan_author_action(request, action, revision, facts, actor)?;
    let void = if action == AuthorAction::RequestReview {
        plan_void(request, revision, &old_status, actor)
            .await?
            .map(|(reviewers, force)| (transaction_phid(), reviewers, force))
    } else {
        None
    };
    Ok((Some((action, transaction_phid())), void))
}

/// The planned title and text field edits.
type FieldEdits = (
    Option<(String, String)>,
    Option<(TextField, String, String, String)>,
);

/// The new title and the new summary or test plan, with their
/// transactions.
async fn plan_fields(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &mut Revision,
    old_text: Option<&(TextField, String)>,
) -> Result<FieldEdits, MethodError> {
    let text = match &wanted.text {
        Some((field, new)) => {
            let old = old_text
                .as_ref()
                .map(|(_, old)| old.clone())
                .unwrap_or_default();
            Some((*field, transaction_phid(), old, new.clone()))
        }
        None => None,
    };
    let title = match wanted.title.clone() {
        Some(title) => {
            let old = plan_title(request, revision, title).await?;
            Some((transaction_phid(), old))
        }
        None => None,
    };
    Ok((title, text))
}

/// The acting user.
fn actor_phid(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    let phid = request.viewer.phid.as_ref();
    let phid = phid.ok_or(MethodError::Unsupported("edits by the omnipotent user"))?;
    Ok(phid.as_str().to_owned())
}

/// `updateReviewStatus()`, once the reviewers changed: the status
/// transaction and the old status.
fn plan_status(request: &ApiRequest<'_>, revision: &mut Revision) -> Option<(String, String)> {
    let status = new_review_status(revision, sticky_accept(request))?;
    if status == revision.status {
        return None;
    }
    let old = std::mem::replace(&mut revision.status, status);
    Some((transaction_phid(), old))
}

/// The revisions the edit links to: the parents or children it edits,
/// the dependencies its summary or test plan declares, then the revisions
/// it mentions.
async fn plan_links(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &Revision,
) -> Result<Vec<Link>, MethodError> {
    let stack = match &wanted.stack {
        Some((edge_type, edit)) => Some(stack::plan(request, revision, *edge_type, edit).await?),
        None => None,
    };
    // expandCustomRemarkupBlockTransactions(): the dependencies, then the
    // reverts, neither of which the texts then mention.
    let (dependency, mut unmentionable) = match &wanted.text {
        Some((_, text)) => dependencies::plan(request, revision, text).await?,
        None => (None, Vec::new()),
    };
    let (revert, reverted) = match &wanted.text {
        Some((_, text)) => reverts::plan(request, revision, text).await?,
        None => (None, Vec::new()),
    };
    unmentionable.extend(reverted);
    let mention =
        plan_object_mention(request, revision, &new_texts(wanted), &unmentionable).await?;
    Ok(stack
        .into_iter()
        .chain(dependency)
        .chain(revert)
        .chain(mention)
        .collect())
}

/// The planned reviewer, subscriber and tag edits.
type ListEdits = (
    Option<reviewers::Planned>,
    Option<Subscription>,
    Option<tags::Planned>,
);

/// The reviewers, the subscribers and the tags added, removed or set.
async fn plan_lists(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    subscription: &mut Option<Subscription>,
    revision: &mut Revision,
    facts: &mut Facts,
) -> Result<ListEdits, MethodError> {
    let reviewer_edit = plan_reviewer_edit(request, wanted, revision, facts).await?;
    let mut subscriber_edit = match &wanted.subscribers {
        Some(edit) => {
            let actor = actor_phid(request)?;
            let implied = subscription.take();
            Some(subscribers::plan(request, edit, implied, revision, facts, &actor).await?)
        }
        None => None,
    };
    // An implicit CC merged with an explicit edit stays first.
    if subscriber_edit.as_ref().is_some_and(|edit| edit.first) {
        *subscription = subscriber_edit.take();
    }
    let tag_edit = match &wanted.projects {
        Some(edit) => Some(tags::plan(request, edit, revision, facts).await?),
        None => None,
    };
    Ok((reviewer_edit, subscriber_edit, tag_edit))
}

/// The reviewers added, removed or set, with their transaction.
async fn plan_reviewer_edit(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &mut Revision,
    facts: &mut Facts,
) -> Result<Option<reviewers::Planned>, MethodError> {
    // The subscription was planned with the reviewers before, as PHP's
    // isAutomaticallySubscribed() sees them.
    let Some(edit) = &wanted.reviewers else {
        return Ok(None);
    };
    Ok(Some(
        reviewers::plan(request, edit, revision, &mut facts.reviewers).await?,
    ))
}

/// `queueMergeConflictChecks()` of the editor and of the linked
/// revisions' sub-editors: abandoning a revision, or re-wiring its stack,
/// rechecks it, when its repository is checked.
async fn plan_merge_checks(
    request: &ApiRequest<'_>,
    revision: &Revision,
    actions: &Actions,
    links: &[Link],
) -> Result<Vec<String>, MethodError> {
    let mut checks = Vec::new();
    let rewiring = links
        .iter()
        .filter(|link| merge_checks::rewires_stack(link.edge_type));
    for link in rewiring.clone() {
        for target in &link.targets {
            if let Linked::Revision { revision, .. } = &target.object
                && merge_checks::enabled(request, revision).await?
            {
                checks.push(revision.phid.as_str().to_owned());
            }
        }
    }
    let abandons = matches!(actions.author_action, Some((AuthorAction::Abandon, _)));
    if (abandons || rewiring.count() > 0) && merge_checks::enabled(request, revision).await? {
        checks.push(revision.phid.as_str().to_owned());
    }
    Ok(checks)
}

async fn plan(request: &ApiRequest<'_>, finder: Option<FileFinder>) -> Result<Plan, MethodError> {
    let wanted = wanted(request)?;
    let mut revision = load_revision(request).await?;
    let actor = actor_phid(request)?;
    let database = &request.context.database;
    let mut facts = Facts::load(database, &revision).await?;
    check_side_effects(request, &revision, &facts).await?;
    let old_text = match &wanted.text {
        Some((field, new)) => Some((*field, plan_text(request, &revision, *field, new).await?)),
        None => None,
    };
    let mut subscription = plan_subscription(
        request,
        &revision,
        &mut facts,
        (&wanted, old_text.as_ref()),
        &actor,
    )
    .await?;
    let review = match wanted.review {
        Some(review) => {
            let updates = plan_review(request, review, &mut revision, &mut facts, &actor).await?;
            Some((review, transaction_phid(), updates))
        }
        None => None,
    };
    let (author_action, void) = match wanted.author_action {
        Some(action) => plan_author(request, action, &mut revision, &facts, &actor).await?,
        None => (None, None),
    };
    let (title, text) = plan_fields(request, &wanted, &mut revision, old_text.as_ref()).await?;
    let (reviewer_edit, subscriber_edit, tag_edit) = plan_lists(
        request,
        &wanted,
        &mut subscription,
        &mut revision,
        &mut facts,
    )
    .await?;
    let links = plan_links(request, &wanted, &revision).await?;
    let old = old_text.as_ref().map(|(_, old)| old.as_str());
    let texts = (&new_texts(&wanted)[..], old);
    let file_edit = files::plan(request, finder, revision.phid.as_str(), texts).await?;
    let comment = wanted.comment.map(|text| {
        (
            text,
            transaction_phid(),
            Phid::generate(PhidType::TRANSACTION_COMMENT, None).into_string(),
        )
    });
    let status = plan_status(request, &mut revision);
    // The subscription first (array_unshift()), the actions, the comment
    // (sortTransactions()), then the status (applyFinalEffects()).
    // A subscription of mentions alone comes before the comment instead.
    let (first, late) = match &subscription {
        Some(subscription) if subscription.first => (Some(subscription.phid.clone()), None),
        Some(subscription) => (None, Some(subscription.phid.clone())),
        None => (None, None),
    };
    let actions = Actions {
        order: wanted.order,
        review,
        author_action,
        void,
        title,
        text,
        reviewer_edit,
        subscriber_edit,
        tag_edit,
    };
    let applied_phids: Vec<String> = first
        .into_iter()
        .chain(actions.phids())
        .chain(late)
        .chain(links.iter().map(|link| link.phid.clone()))
        .chain(file_edit.iter().map(|edit| edit.phid.clone()))
        .chain(comment.iter().map(|(_, phid, _)| phid.clone()))
        .chain(status.iter().map(|(phid, _)| phid.clone()))
        .collect();
    let merge_checks = plan_merge_checks(request, &revision, &actions, &links).await?;
    let input = herald_input(&actor, &actions, subscription.as_ref());
    let published = publication(request, &mut revision, &mut facts, &input, &applied_phids).await?;
    let tasks = link_tasks(database, links, &actor, &published.must_encrypt).await?;
    Ok(Plan {
        actor,
        group_id: random::random_characters(32),
        subscription,
        actions,
        links: tasks,
        file_edit,
        merge_checks,
        comment,
        status,
        applied_phids,
        rules: published.rules,
        transcript: published.transcript,
        herald_header: published.herald_header,
        worker_data: published.worker_data,
        herald: published.herald,
        revision,
    })
}

/// Inserts a transaction without a comment on the edited revision.
async fn insert_transaction(
    request: &ApiRequest<'_>,
    plan: &Plan,
    connection: &mut phab_db::sqlx::MySqlConnection,
    row: (&str, &str, &str, &str, &str),
) -> Result<(), MethodError> {
    let object = plan.revision.phid.as_str();
    insert_transaction_on(request, plan, object, connection, row).await
}

/// Inserts a transaction without a comment on `object`.
async fn insert_transaction_on(
    request: &ApiRequest<'_>,
    plan: &Plan,
    object: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
    (phid, kind, old, new, metadata): (&str, &str, &str, &str, &str),
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let table = match Phid::new(object).phid_type() {
        PhidType::COMMIT => database.table("audit", "audit_transaction"),
        _ => database.table("differential", "differential_transaction"),
    };
    phab_db::sqlx::query(&format!(
        "INSERT INTO {table} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, \
         commentPHID, commentVersion, transactionType, oldValue, newValue, contentSource, \
         metadata, dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, NULL, 0, ?, ?, ?, \
         ?, ?, ?, ?)"
    ))
    .bind(phid)
    .bind(&plan.actor)
    .bind(object)
    .bind(&plan.actor)
    .bind(kind)
    .bind(old)
    .bind(new)
    .bind(CONDUIT_SOURCE)
    .bind(metadata)
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Writes the planned edit in one transaction.
async fn write(request: &ApiRequest<'_>, plan: &Plan) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    let revision = &plan.revision;
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET status = ?, title = ?, dateModified = ? WHERE id = ?",
        database.table("differential", "differential_revision")
    ))
    .bind(&revision.status)
    .bind(&revision.title)
    .bind(now)
    .bind(revision.id)
    .execute(&mut *connection)
    .await?;
    let metadata = php::json_encode(&json!({"core.groupID": plan.group_id}));
    // In the order PHP saves them: an implicit CC first, the actions, then
    // a subscription of mentions alone.
    let subscription = plan.subscription.as_ref();
    if let Some(subscription) = subscription.filter(|subscription| subscription.first) {
        write_subscription(request, plan, subscription, &metadata, connection).await?;
    }
    write_actions(request, plan, &metadata, connection).await?;
    if let Some(subscription) = subscription.filter(|subscription| !subscription.first) {
        write_subscription(request, plan, subscription, &metadata, connection).await?;
    }
    // The edge transaction is saved with the others, before the comment;
    // the edges and the inverse edit come with the external effects.
    for (link, _) in &plan.links {
        write_link_transaction(request, plan, link, connection).await?;
    }
    if let Some(edit) = &plan.file_edit {
        files::write(request, plan, edit, connection).await?;
    }
    if let Some((text, phid, comment_phid)) = &plan.comment {
        write_comment(
            request,
            plan,
            (text, phid, comment_phid),
            &metadata,
            connection,
        )
        .await?;
    }
    for (link, _) in &plan.links {
        write_link(request, plan, link, connection).await?;
    }
    if let Some((phid, old)) = &plan.status {
        // populateTransaction() sets no group.
        let (old, new) = (
            php::json_encode(&json!(old)),
            php::json_encode(&json!(revision.status)),
        );
        let row = (
            phid.as_str(),
            "differential.revision.status",
            &*old,
            &*new,
            "[]",
        );
        insert_transaction(request, plan, connection, row).await?;
    }
    merge_checks::queue(request, connection, &plan.merge_checks).await?;
    let transcript_id = write_herald(request, plan, connection).await?;
    herald_actions::write(request, plan, transcript_id, connection).await?;
    queue_publishing(request, plan, connection).await?;
    transaction.commit().await?;
    Ok(())
}

/// The transactions of the actions and field edits, with their effects.
async fn write_actions(
    request: &ApiRequest<'_>,
    plan: &Plan,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    for action in &plan.actions.order {
        write_action(request, plan, *action, metadata, connection).await?;
    }
    if let Some((phid, reviewers, force)) = &plan.actions.void {
        write_void(request, plan, (phid, reviewers, force), connection).await?;
    }
    Ok(())
}

/// The summary or test plan, and its transaction.
async fn write_text(
    request: &ApiRequest<'_>,
    plan: &Plan,
    (field, phid, old, new): &(TextField, String, String, String),
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET {} = ? WHERE id = ?",
        request
            .context
            .database
            .table("differential", "differential_revision"),
        field.column()
    ))
    .bind(new)
    .bind(plan.revision.id)
    .execute(&mut *connection)
    .await?;
    let (old, new) = (php::json_encode(&json!(old)), php::json_encode(&json!(new)));
    let row = (
        phid.as_str(),
        field.transaction_type(),
        &*old,
        &*new,
        metadata,
    );
    insert_transaction(request, plan, connection, row).await
}

/// The transaction of one action, and its effect.
async fn write_action(
    request: &ApiRequest<'_>,
    plan: &Plan,
    action: Action,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = &plan.revision;
    let actions = &plan.actions;
    match action {
        Action::Review => {
            if let Some((review, phid, updates)) = &actions.review {
                let row = (
                    phid.as_str(),
                    review.transaction_type(),
                    "false",
                    "true",
                    metadata,
                );
                insert_transaction(request, plan, connection, row).await?;
                write_reviewers(request, plan, *review, updates, connection).await?;
            }
        }
        Action::Author => {
            if let Some((action, phid)) = &actions.author_action {
                let row = (
                    phid.as_str(),
                    action.transaction_type(),
                    "false",
                    "true",
                    metadata,
                );
                insert_transaction(request, plan, connection, row).await?;
            }
        }
        Action::Title => {
            if let Some((phid, old)) = &actions.title {
                let (old, new) = (
                    php::json_encode(&json!(old)),
                    php::json_encode(&json!(revision.title)),
                );
                let row = (
                    phid.as_str(),
                    "differential.revision.title",
                    &*old,
                    &*new,
                    metadata,
                );
                insert_transaction(request, plan, connection, row).await?;
            }
        }
        Action::Text => {
            if let Some(text) = &actions.text {
                write_text(request, plan, text, metadata, connection).await?;
            }
        }
        Action::Reviewers => {
            if let Some(edit) = &actions.reviewer_edit {
                let (old, new) = (reviewers::encode(&edit.old), reviewers::encode(&edit.new));
                let row = (
                    edit.phid.as_str(),
                    "differential.revision.reviewers",
                    &*old,
                    &*new,
                    metadata,
                );
                insert_transaction(request, plan, connection, row).await?;
                reviewers::write(
                    connection,
                    database,
                    revision.phid.as_str(),
                    edit,
                    request.now,
                )
                .await?;
            }
        }
        Action::Subscribers => {
            if let Some(edit) = &actions.subscriber_edit {
                write_subscription(request, plan, edit, metadata, connection).await?;
            }
        }
        Action::Projects => {
            if let Some(edit) = &actions.tag_edit {
                tags::write(request, plan, edit, connection).await?;
            }
        }
        // Written with the links.
        Action::Stack => {}
    }
    Ok(())
}

/// The metadata of an edge transaction of the edit's group.
fn edge_metadata(plan: &Plan, edge_type: i64) -> String {
    php::json_encode(&json!({"edge:type": edge_type, "core.groupID": plan.group_id}))
}

/// The transaction of a link's edge, in the slim format of edges: the
/// removed and the added PHIDs (`getModernNewEdgeTransactionData()`).
async fn write_link_transaction(
    request: &ApiRequest<'_>,
    plan: &Plan,
    link: &Link,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let (old, new) = (
        php::json_encode(&json!(link.removed)),
        php::json_encode(&json!(link.added)),
    );
    let metadata = edge_metadata(plan, link.edge_type);
    let row = (link.phid.as_str(), "core:edge", &*old, &*new, &*metadata);
    insert_transaction(request, plan, connection, row).await
}

/// A link's edges, the new ones numbered in the order of their targets,
/// and the inverse edit of each other revision: its transaction and
/// `dateModified`.
async fn write_link(
    request: &ApiRequest<'_>,
    plan: &Plan,
    link: &Link,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.phid.as_str();
    for removed in &link.removed {
        delete_edge(database, connection, (revision, link.edge_type, removed)).await?;
    }
    for (seq, added) in (0..).zip(&link.added) {
        let edge = (revision, link.edge_type, added.as_str());
        insert_edge(database, connection, edge, seq, request.now).await?;
    }
    let source = php::json_encode(&json!([revision]));
    let inverse = edge_metadata(plan, inverse_edge_type(link.edge_type)?);
    for target in &link.targets {
        let phid = target.phid();
        let (old, new) = if target.added {
            ("[]", &*source)
        } else {
            (&*source, "[]")
        };
        let row = (
            target.inverse_phid.as_str(),
            "core:edge",
            old,
            new,
            &*inverse,
        );
        insert_transaction_on(request, plan, phid, connection, row).await?;
        // The audit editor saves commits as they are.
        let Linked::Revision { revision, .. } = &target.object else {
            continue;
        };
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET dateModified = ? WHERE id = ?",
            database.table("differential", "differential_revision")
        ))
        .bind(request.now)
        .bind(revision.id)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// `PhabricatorUnsubscribedFromObjectEdgeType`.
const UNSUBSCRIBED_FROM_OBJECT: i64 = 24;

/// The inverse of an edge type the editor writes.
fn inverse_edge_type(edge_type: i64) -> Result<i64, MethodError> {
    match edge_type {
        edges::OBJECT_HAS_SUBSCRIBER => Ok(edges::SUBSCRIBED_TO_OBJECT),
        edges::OBJECT_HAS_UNSUBSCRIBER => Ok(UNSUBSCRIBED_FROM_OBJECT),
        edges::OBJECT_HAS_PROJECT => Ok(PROJECT_HAS_OBJECT),
        edges::OBJECT_MENTIONS_OBJECT => Ok(edges::OBJECT_MENTIONED_BY_OBJECT),
        edges::REVISION_DEPENDS_ON_REVISION => Ok(edges::REVISION_DEPENDED_ON_BY_REVISION),
        edges::REVISION_DEPENDED_ON_BY_REVISION => Ok(edges::REVISION_DEPENDS_ON_REVISION),
        reverts::REVERTS => Ok(reverts::REVERTED_BY),
        reverts::REVERTED_BY => Ok(reverts::REVERTS),
        _ => Err(MethodError::internal("an edge type without inverse")),
    }
}

/// `PhabricatorEdgeEditor::addEdge()`: the edge, numbered `seq` among its
/// source's new edges, and its inverse, in its destination's database.
async fn insert_edge(
    database: &Database,
    connection: &mut phab_db::sqlx::MySqlConnection,
    (src, edge_type, dst): (&str, i64, &str),
    seq: i64,
    now: i64,
) -> Result<(), MethodError> {
    let inverse = inverse_edge_type(edge_type)?;
    for (src, edge_type, dst, seq) in [(src, edge_type, dst, seq), (dst, inverse, src, 0)] {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (src, type, dst, dateCreated, seq, dataID) VALUES (?, ?, ?, ?, ?, \
             NULL) ON DUPLICATE KEY UPDATE dataID = VALUES(dataID)",
            database.table(lists::edge_database(src)?, "edge")
        ))
        .bind(src)
        .bind(edge_type)
        .bind(dst)
        .bind(now)
        .bind(seq)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// `PhabricatorEdgeEditor::removeEdge()`: the edge and its inverse.
async fn delete_edge(
    database: &Database,
    connection: &mut phab_db::sqlx::MySqlConnection,
    (src, edge_type, dst): (&str, i64, &str),
) -> Result<(), MethodError> {
    let inverse = inverse_edge_type(edge_type)?;
    for (src, edge_type, dst) in [(src, edge_type, dst), (dst, inverse, src)] {
        phab_db::sqlx::query(&format!(
            "DELETE FROM {} WHERE src = ? AND type = ? AND dst = ?",
            database.table(lists::edge_database(src)?, "edge")
        ))
        .bind(src)
        .bind(edge_type)
        .bind(dst)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// The void transaction and its effect on the reviewers.
async fn write_void(
    request: &ApiRequest<'_>,
    plan: &Plan,
    (phid, reviewers, force): (&str, &[(i64, String)], &[&str]),
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let metadata = php::json_encode(&json!({
        "void.force": force,
        "core.groupID": plan.group_id,
    }));
    // mpull() keeps the rows' IDs as keys.
    let new = php::json_encode(&Value::Object(
        reviewers
            .iter()
            .map(|(id, phid)| (id.to_string(), json!(phid)))
            .collect(),
    ));
    let row = (
        phid,
        "differential.revision.void",
        "false",
        &*new,
        &*metadata,
    );
    insert_transaction(request, plan, connection, row).await?;
    phab_db::bind_all(
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET voidedPHID = ? WHERE revisionPHID = ? AND voidedPHID IS NULL AND \
             reviewerPHID IN ({})",
            database.table("differential", "differential_reviewer"),
            phab_db::placeholders(reviewers.len())
        ))
        .bind(&plan.actor)
        .bind(plan.revision.phid.as_str()),
        reviewers.iter().map(|(_, phid)| phid.as_str()),
    )
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The publish tasks: the revision's, then the sub-editors'.
async fn queue_publishing(
    request: &ApiRequest<'_>,
    plan: &Plan,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let publish = |data, object_phid| worker::Task {
        class: "PhabricatorApplicationTransactionPublishWorker",
        data,
        priority: worker::PRIORITY_ALERTS,
        object_phid: Some(object_phid),
    };
    let revision = plan.revision.phid.as_str();
    worker::schedule_task(
        connection,
        database,
        publish(&plan.worker_data, revision),
        request.now,
    )
    .await?;
    for (link, data) in &plan.links {
        for (target, data) in link.targets.iter().zip(data) {
            let target = target.phid();
            worker::schedule_task(connection, database, publish(data, target), request.now).await?;
        }
    }
    Ok(())
}

/// The comment, its transaction, and `markReviewerComments()`.
async fn write_comment(
    request: &ApiRequest<'_>,
    plan: &Plan,
    (text, phid, comment_phid): (&str, &str, &str),
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let revision = &plan.revision;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
         commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
         dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, ?, 1, 'core:comment', 'null', \
         'null', ?, ?, ?, ?)",
        database.table("differential", "differential_transaction")
    ))
    .bind(phid)
    .bind(&plan.actor)
    .bind(revision.phid.as_str())
    .bind(&plan.actor)
    .bind(comment_phid)
    .bind(CONDUIT_SOURCE)
    .bind(metadata)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
         commentVersion, content, contentSource, isDeleted, dateCreated, dateModified, \
         revisionPHID, changesetID, isNewFile, lineNumber, lineLength, fixedState, hasReplies, \
         replyToCommentPHID, attributes) VALUES (?, ?, ?, 'public', ?, 1, ?, ?, 0, ?, ?, NULL, \
         NULL, 0, 0, 0, NULL, 0, NULL, '[]')",
        database.table("differential", "differential_transaction_comment")
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
    if let Some(diff) = &revision.active_diff_phid {
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET lastCommentDiffPHID = ? WHERE revisionPHID = ? AND reviewerPHID = ?",
            database.table("differential", "differential_reviewer")
        ))
        .bind(diff.as_str())
        .bind(revision.phid.as_str())
        .bind(&plan.actor)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// The reviewer rows a review writes (`applyReviewerEffect()`).
async fn write_reviewers(
    request: &ApiRequest<'_>,
    plan: &Plan,
    review: Review,
    updates: &[ReviewerUpdate],
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = &plan.revision;
    for update in updates {
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET reviewerStatus = ?, dateModified = ?, \
             lastActionDiffPHID = COALESCE(?, lastActionDiffPHID), \
             lastActorPHID = IF(?, ?, lastActorPHID), voidedPHID = NULL, options = ? \
             WHERE revisionPHID = ? AND reviewerPHID = ?",
            database.table("differential", "differential_reviewer")
        ))
        .bind(review.status())
        .bind(request.now)
        .bind(revision.active_diff_phid.as_ref().map(Phid::as_str))
        .bind(update.status_changed)
        .bind(&plan.actor)
        .bind(&update.options)
        .bind(revision.phid.as_str())
        .bind(&update.reviewer)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// A `core:subscribers` transaction and its edges
/// (`PhabricatorSubscriptionsEditor::save()`): who it subscribes loses
/// their "unsubscribed" edge, who it unsubscribes gets one.
async fn write_subscription(
    request: &ApiRequest<'_>,
    plan: &Plan,
    subscription: &Subscription,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let (old, new) = (&subscription.old, &subscription.new);
    let (old_value, new_value) = (php::json_encode(&json!(old)), php::json_encode(&json!(new)));
    let row = (
        subscription.phid.as_str(),
        "core:subscribers",
        &*old_value,
        &*new_value,
        metadata,
    );
    insert_transaction(request, plan, connection, row).await?;
    write_subscription_edges(request, plan, subscription, connection).await
}

/// The subscription's edges: `PhabricatorEdgeEditor` numbers the new edges
/// of each source.
async fn write_subscription_edges(
    request: &ApiRequest<'_>,
    plan: &Plan,
    subscription: &Subscription,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.phid.as_str();
    let (old, new) = (&subscription.old, &subscription.new);
    let changes = lists::minus(new, old)
        .into_iter()
        .map(|phid| {
            (
                phid,
                edges::OBJECT_HAS_UNSUBSCRIBER,
                edges::OBJECT_HAS_SUBSCRIBER,
            )
        })
        .chain(lists::minus(old, new).into_iter().map(|phid| {
            (
                phid,
                edges::OBJECT_HAS_SUBSCRIBER,
                edges::OBJECT_HAS_UNSUBSCRIBER,
            )
        }));
    for (seq, (phid, gone, added)) in (0_i64..).zip(changes) {
        delete_edge(database, connection, (revision, gone, &phid)).await?;
        insert_edge(
            database,
            connection,
            (revision, added, &phid),
            seq,
            request.now,
        )
        .await?;
    }
    Ok(())
}

/// The transcript, the rules' applied state and the saved header.
/// The transcript, the rules' bookkeeping and the saved header. Returns the
/// transcript's ID, which the Herald editor's transactions name.
async fn write_herald(
    request: &ApiRequest<'_>,
    plan: &Plan,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<u64, MethodError> {
    let database = &request.context.database;
    let revision = &plan.revision;
    let transcript = &plan.transcript;
    let transcript_id = phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, time, host, duration, objectPHID, dryRun, objectTranscript, \
         ruleTranscripts, conditionTranscripts, applyTranscripts, garbageCollected) VALUES \
         (?, ?, ?, ?, ?, 0, ?, ?, ?, ?, 0)",
        database.table("herald", "herald_transcript")
    ))
    .bind(Phid::generate(PhidType::HERALD_TRANSCRIPT, None).as_str())
    .bind(request.now)
    .bind(&request.context.hostname)
    .bind(transcript.duration)
    .bind(&transcript.object_phid)
    .bind(&transcript.object_transcript)
    .bind(&transcript.rule_transcripts)
    .bind(&transcript.condition_transcripts)
    .bind(&transcript.apply_transcripts)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    // Rules repeating on every edit forget where they applied.
    if !plan.rules.is_empty() {
        phab_db::bind_all(
            phab_db::sqlx::query(&format!(
                "DELETE FROM {} WHERE phid = ? AND ruleID IN ({})",
                database.table("herald", "herald_ruleapplied"),
                phab_db::placeholders(plan.rules.len())
            ))
            .bind(revision.phid.as_str()),
            plan.rules.iter().map(|rule| rule.id.as_str()),
        )
        .execute(&mut *connection)
        .await?;
    }
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, header) VALUES (?, ?) ON DUPLICATE KEY UPDATE header = \
         VALUES(header)",
        database.table("herald", "herald_savedheader")
    ))
    .bind(revision.phid.as_str())
    .bind(&plan.herald_header)
    .execute(&mut *connection)
    .await?;
    Ok(transcript_id)
}

pub use files::{FileFinder, FilesFuture};

/// `differential.revision.edit`. Without a [`FileFinder`], references to
/// files are left to PHP.
#[derive(Default)]
pub struct RevisionEdit {
    files: Option<FileFinder>,
}

impl RevisionEdit {
    /// The method, which finds referenced files with `files`.
    pub fn with_files(files: FileFinder) -> Self {
        Self { files: Some(files) }
    }
}

impl Method for RevisionEdit {
    fn name(&self) -> &'static str {
        "differential.revision.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let plan = plan(request, self.files).await?;
            write(request, &plan).await?;
            Ok(json!({
                "object": {"id": plan.revision.id, "phid": plan.revision.phid.as_str()},
                "transactions": plan
                    .applied_phids
                    .iter()
                    .map(|phid| json!({"phid": phid}))
                    .collect::<Vec<_>>(),
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_monograms_like_php() {
        assert_eq!(rule_monograms("<1>, <2>"), json!(["H1", "H2"]));
        assert_eq!(rule_monograms("none"), json!([]));
    }
}
