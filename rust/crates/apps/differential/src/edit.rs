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
//! Ported, on an existing revision the actor can see: a `comment` without
//! remarkup that mentions or references anything, one review or author
//! action as the defaults or a new `title`, or a comment and one of those;
//! with the review status change they cause, when Herald's rules are ones
//! [`phab_app_herald::engine`] evaluates. A comment subscribes its author
//! when nothing else does (`applyImplicitCC()`).

use phab_app_herald::engine::{self, Adapter, Applied, Evaluation, Rule, Subject};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php, random};
use phab_db::phpser::Value as Php;
use phab_db::{Database, edges, rows, worker};
use serde_json::{Value, json};

use crate::search::{self, Constraints, Order, Revisions};
use crate::storage::Revision;

/// The content source of a Conduit call (`PhabricatorConduitContentSource`).
const CONDUIT_SOURCE: &str = r#"{"source":"conduit","params":[]}"#;

/// `PhabricatorProjectObjectHasWatcherEdgeType`.
const PROJECT_HAS_WATCHER: i64 = 47;

/// The Herald adapter of revisions (`HeraldDifferentialRevisionAdapter`),
/// for the fields and actions Rust implements.
struct RevisionAdapter;

impl Adapter for RevisionAdapter {
    fn has_field(&self, key: &str) -> bool {
        matches!(key, "new-object" | "rule" | "projects")
    }

    fn has_action(&self, action: &str) -> bool {
        matches!(action, "email.must-encrypt" | "projects.add")
    }
}

/// Whether remarkup could expand into other transactions: mentions,
/// object references, files, projects. Only text without any of their
/// markers is ported.
fn has_references(text: &str) -> bool {
    if text
        .chars()
        .any(|character| matches!(character, '{' | '#' | '[' | '<' | '/' | ':'))
    {
        return true;
    }
    // T4, rXYZabc, F1, H2, and the like; `@user2` mentions a user and
    // `D123` a revision, which are ported.
    let mut previous = ' ';
    let mut word = String::new();
    let mut word_start = ' ';
    for character in text.chars().chain([' ']) {
        if character.is_ascii_alphanumeric() {
            if word.is_empty() {
                word_start = previous;
            }
            word.push(character);
        } else if !word.is_empty() {
            let revision = word
                .strip_prefix('D')
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
            if word.bytes().any(|b| b.is_ascii_digit()) && word_start != '@' && !revision {
                return true;
            }
            word.clear();
        }
        previous = character;
    }
    false
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

/// What the `transactions` parameter asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Wanted {
    /// A comment's text.
    comment: Option<String>,
    /// A review, for the reviewers the actor has authority over.
    review: Option<Review>,
    /// An action of the author.
    author_action: Option<AuthorAction>,
    /// A new title.
    title: Option<String>,
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
                if is_empty_comment(text) || has_references(text) {
                    return Err(MethodError::Unsupported(
                        "empty comments and comments with references",
                    ));
                }
                wanted.comment = Some(text.clone());
            }
            // Reviewing as the defaults; a list of reviewers is left to PHP.
            (Some("accept"), Some(Value::Bool(true))) if wanted.review.is_none() => {
                wanted.review = Some(Review::Accept);
            }
            (Some("reject"), Some(Value::Bool(true))) if wanted.review.is_none() => {
                wanted.review = Some(Review::Reject);
            }
            (Some("resign"), Some(Value::Bool(true))) if wanted.review.is_none() => {
                wanted.review = Some(Review::Resign);
            }
            (Some("plan-changes"), Some(Value::Bool(true))) if wanted.author_action.is_none() => {
                wanted.author_action = Some(AuthorAction::PlanChanges);
            }
            (Some("abandon"), Some(Value::Bool(true))) if wanted.author_action.is_none() => {
                wanted.author_action = Some(AuthorAction::Abandon);
            }
            (Some("request-review"), Some(Value::Bool(true))) if wanted.author_action.is_none() => {
                wanted.author_action = Some(AuthorAction::RequestReview);
            }
            (Some("reclaim"), Some(Value::Bool(true))) if wanted.author_action.is_none() => {
                wanted.author_action = Some(AuthorAction::Reclaim);
            }
            (Some("title"), Some(Value::String(title))) if wanted.title.is_none() => {
                wanted.title = Some(title.clone());
            }
            _ => return Err(MethodError::Unsupported("transactions not ported")),
        }
    }
    if wanted == Wanted::default() {
        return Err(MethodError::Unsupported("empty transaction lists"));
    }
    // The order of several actions is the request's; one is ported.
    let actions = usize::from(wanted.review.is_some())
        + usize::from(wanted.author_action.is_some())
        + usize::from(wanted.title.is_some());
    if actions > 1 {
        return Err(MethodError::Unsupported("several actions"));
    }
    Ok(wanted)
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
        Ok(Self {
            mentioned: json!([]),
            should_broadcast,
            reviewers,
            projects: destinations(database, &revision.phid, edges::OBJECT_HAS_PROJECT).await?,
            subscribers: destinations(database, &revision.phid, edges::OBJECT_HAS_SUBSCRIBER)
                .await?,
            muted: destinations(database, &revision.phid, edges::MUTED_BY).await?,
        })
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
    let projects: Vec<Phid> = facts.projects.iter().cloned().map(Phid::new).collect();
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
    /// The subscribers before.
    old: Vec<String>,
    /// Who it subscribes, in order.
    added: Vec<String>,
    /// Whether it comes first: an implicit CC is unshifted, a subscription
    /// of mentions alone is appended.
    first: bool,
}

/// `extractPHIDsFromMentions()` of a comment: the mentioned users, as the
/// people query returns them (newest first), with whether each is a bot,
/// an administrator, and an active account.
async fn mentioned_users(
    database: &Database,
    text: &str,
) -> Result<Vec<(String, bool, bool, bool)>, MethodError> {
    let engine = phab_remarkup::phabricator::engine(phab_remarkup::phabricator::default_config());
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
    wanted: &Wanted,
    actor: &str,
) -> Result<Option<Subscription>, MethodError> {
    let Some(text) = &wanted.comment else {
        return Ok(None);
    };
    let context = request.context;
    let database = &context.database;
    let implicit = implies_subscription(database, revision, facts, actor).await?;
    let mentioned = mentioned_users(database, text).await?;
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
    Ok(Some(Subscription {
        phid: transaction_phid(),
        old,
        added,
        first: implicit,
    }))
}

/// A revision a comment mentions (`expandRemarkupBlockTransactions()`):
/// the `core:edge` transaction of the "mentions" edge, and the inverse
/// edit of the mentioned revision (`applyInverseEdgeTransactions()`), a
/// sub-editor which runs no Herald.
struct ObjectMention {
    /// The edge transaction and the mentioned objects before.
    phid: String,
    old: Vec<String>,
    /// The mentioned revision, its facts, its transaction and the objects
    /// which mentioned it before.
    target: Revision,
    target_facts: Facts,
    inverse_phid: String,
    inverse_old: Vec<String>,
}

/// The revisions `text` references, as `D123`; other references are left
/// to PHP, and so are several revisions.
async fn plan_object_mention(
    request: &ApiRequest<'_>,
    revision: &Revision,
    text: &str,
) -> Result<Option<ObjectMention>, MethodError> {
    let engine = phab_remarkup::phabricator::engine(phab_remarkup::phabricator::default_config());
    let references = engine
        .prepare(text.as_bytes(), phab_remarkup::Mode::Html)
        .object_references();
    if references.is_empty() {
        return Ok(None);
    }
    let mut ids: Vec<i64> = Vec::new();
    for (prefix, id) in references {
        let id = std::str::from_utf8(&id).ok().and_then(|id| id.parse().ok());
        match (prefix, id) {
            ("D", Some(id)) if !ids.contains(&id) => ids.push(id),
            ("D", Some(_)) => {}
            _ => return Err(MethodError::Unsupported("references to other objects")),
        }
    }
    let query = Revisions {
        request,
        constraints: Constraints {
            ids: Some(ids),
            ..Constraints::default()
        },
        order: Order::Updated,
        memory: std::sync::Mutex::default(),
    };
    // Objects don't mention themselves.
    let mut found: Vec<Revision> = phab_conduit::pager::execute(
        &query,
        None,
        phab_conduit::pager::Direction::Forward,
        usize::MAX,
    )
    .await?
    .into_iter()
    .filter(|found| found.phid != revision.phid)
    .collect();
    let target = match found.len() {
        0 => return Ok(None),
        1 => found.remove(0),
        _ => return Err(MethodError::Unsupported("references to several revisions")),
    };
    let database = &request.context.database;
    let old = destinations(database, &revision.phid, edges::OBJECT_MENTIONS_OBJECT).await?;
    // Mentioning again has no effect.
    if old.iter().any(|phid| phid == target.phid.as_str()) {
        return Ok(None);
    }
    let target_facts = Facts::load(database, &target).await?;
    check_side_effects(request, &target, &target_facts).await?;
    if new_review_status(&target, sticky_accept(request))
        .is_some_and(|status| status != target.status)
    {
        return Err(MethodError::Unsupported(
            "mentioned revisions with a stale status",
        ));
    }
    let inverse_old =
        destinations(database, &target.phid, edges::OBJECT_MENTIONED_BY_OBJECT).await?;
    Ok(Some(ObjectMention {
        phid: transaction_phid(),
        old,
        target,
        target_facts,
        inverse_phid: transaction_phid(),
        inverse_old,
    }))
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

/// The planned edit.
struct Plan {
    /// The revision as the edit leaves it.
    revision: Revision,
    actor: String,
    /// The `core:subscribers` transaction `applyImplicitCC()` adds: its
    /// PHID and the subscribers before.
    subscription: Option<Subscription>,
    /// The review, its transaction and its reviewers.
    review: Option<(Review, String, Vec<ReviewerUpdate>)>,
    /// The author's action and its transaction.
    author_action: Option<(AuthorAction, String)>,
    /// The `differential.revision.title` transaction, with the old title.
    title: Option<(String, String)>,
    /// The `differential.revision.void` transaction of "Request Review":
    /// its PHID, the reviewers it voids and the statuses it must void.
    void: Option<PlannedVoid>,
    /// The comment: its text, transaction and comment PHIDs.
    comment: Option<(String, String, String)>,
    /// The `differential.revision.status` transaction `updateReviewStatus()`
    /// adds, with the old status.
    status: Option<(String, String)>,
    /// A revision the comment mentions, and its publish task.
    object_mention: Option<(ObjectMention, String)>,
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
    database: &Database,
    revision: &Revision,
    facts: &Facts,
    applied_phids: &[String],
) -> Result<(Vec<Rule>, Evaluation, Vec<Applied>, Vec<String>), MethodError> {
    let unsupported = |error: engine::HeraldError| match error {
        engine::HeraldError::Unsupported(what) => MethodError::Unsupported(what),
        other => MethodError::internal(other),
    };
    let rules = engine::load_rules(database, "differential")
        .await
        .map_err(unsupported)?;
    engine::check_supported(&rules, &RevisionAdapter).map_err(unsupported)?;
    let subject = Subject {
        phid: revision.phid.as_str().to_owned(),
        content_type: "differential".to_owned(),
        name: revision.title.clone(),
        applied_transaction_phids: Some(applied_phids.to_vec()),
    };
    let values = vec![
        ("new-object".to_owned(), Php::Bool(false)),
        ("rule".to_owned(), Php::Null),
        (
            "projects".to_owned(),
            Php::list(facts.projects.iter().map(|phid| Php::str(phid))),
        ),
    ];
    let evaluation = engine::evaluate(&rules, &subject, &values);
    let mut applied = Vec::new();
    let mut must_encrypt = Vec::new();
    for effect in &evaluation.effects {
        match effect.action.as_str() {
            "email.must-encrypt" => {
                must_encrypt.push(effect.rule_phid.clone());
                applied.push(Applied {
                    effect: effect.clone(),
                    applied: true,
                    reasons: vec![(
                        "do.must-encrypt".to_owned(),
                        Php::list([Php::str(&effect.rule_phid)]),
                    )],
                });
            }
            _ => {
                return Err(MethodError::Unsupported(
                    "Herald effects which edit the revision",
                ));
            }
        }
    }
    Ok((rules, evaluation, applied, must_encrypt))
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
    let (to, cc): (Vec<Value>, Vec<Value>) = if facts.should_broadcast {
        let mut to = vec![json!(author)];
        to.extend(reviewers.iter().map(|phid| json!(phid)));
        (
            to,
            facts.subscribers.iter().map(|phid| json!(phid)).collect(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let mut notify: Vec<Value> = Vec::new();
    for phid in to.iter().chain(&cc) {
        if !notify.contains(phid) {
            notify.push(phid.clone());
        }
    }
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
        stamp("phid", "removed", json!([])),
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
/// Abandoning a revision with a repository may queue merge conflict checks,
/// and drafts promote or demote: those are left to PHP.
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
            allowed && !revision.is_closed() && revision.repository_phid.is_none()
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

/// A new transaction PHID of revisions.
fn transaction_phid() -> String {
    Phid::generate(PhidType::TRANSACTION, Some(PhidType::REVISION)).into_string()
}

/// What Herald and the publish task need of the planned edit.
struct Publication {
    must_encrypt: Vec<String>,
    rules: Vec<Rule>,
    transcript: engine::TranscriptRow,
    herald_header: String,
    worker_data: String,
}

/// Runs Herald on the edited revision and builds the publish task.
async fn publication(
    database: &Database,
    revision: &Revision,
    facts: &Facts,
    actor: &str,
    applied_phids: &[String],
) -> Result<Publication, MethodError> {
    let (rules, evaluation, applied, must_encrypt) =
        run_herald(database, revision, facts, applied_phids).await?;
    let subject = Subject {
        phid: revision.phid.as_str().to_owned(),
        content_type: "differential".to_owned(),
        name: revision.title.clone(),
        applied_transaction_phids: Some(applied_phids.to_vec()),
    };
    let transcript = evaluation.transcript(&subject, &applied);
    let herald_header = engine::combine_headers(
        saved_header(database, &revision.phid).await?.as_deref(),
        &engine::rules_header(&applied),
    );
    let state = worker_state(revision, facts, actor, &must_encrypt, &herald_header);
    let worker_data = php::json_encode(&json!({
        "objectPHID": revision.phid.as_str(),
        "actorPHID": actor,
        "xactionPHIDs": applied_phids,
        "state": state,
    }));
    Ok(Publication {
        must_encrypt,
        rules,
        transcript,
        herald_header,
        worker_data,
    })
}

/// The publish task of the mentioned revision, with the Herald outcome
/// `mustEncrypt` its parent editor propagates.
async fn mention_task(
    database: &Database,
    mention: &ObjectMention,
    actor: &str,
    must_encrypt: &[String],
) -> Result<String, MethodError> {
    let header = saved_header(database, &mention.target.phid)
        .await?
        .unwrap_or_else(|| "none".to_owned());
    let state = worker_state(
        &mention.target,
        &mention.target_facts,
        actor,
        must_encrypt,
        &header,
    );
    Ok(php::json_encode(&json!({
        "objectPHID": mention.target.phid.as_str(),
        "actorPHID": actor,
        "xactionPHIDs": [mention.inverse_phid],
        "state": state,
    })))
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

async fn plan(request: &ApiRequest<'_>) -> Result<Plan, MethodError> {
    let wanted = wanted(request)?;
    let mut revision = load_revision(request).await?;
    let actor = request
        .viewer
        .phid
        .as_ref()
        .ok_or(MethodError::Unsupported("edits by the omnipotent user"))?
        .as_str()
        .to_owned();
    let database = &request.context.database;
    let mut facts = Facts::load(database, &revision).await?;
    check_side_effects(request, &revision, &facts).await?;
    let subscription = plan_subscription(request, &revision, &mut facts, &wanted, &actor).await?;
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
    let title = match wanted.title {
        Some(title) => {
            let old = plan_title(request, &mut revision, title).await?;
            Some((transaction_phid(), old))
        }
        None => None,
    };
    let object_mention = match &wanted.comment {
        Some(text) => plan_object_mention(request, &revision, text).await?,
        None => None,
    };
    let comment = wanted.comment.map(|text| {
        (
            text,
            transaction_phid(),
            Phid::generate(PhidType::TRANSACTION_COMMENT, None).into_string(),
        )
    });
    // updateReviewStatus(), once the reviewers changed.
    let status = match new_review_status(&revision, sticky_accept(request)) {
        Some(status) if status != revision.status => {
            let old = std::mem::replace(&mut revision.status, status);
            Some((transaction_phid(), old))
        }
        _ => None,
    };
    // The subscription first (array_unshift()), the actions, the comment
    // (sortTransactions()), then the status (applyFinalEffects()).
    // A subscription of mentions alone comes before the comment instead.
    let (first, late) = match &subscription {
        Some(subscription) if subscription.first => (Some(subscription.phid.clone()), None),
        Some(subscription) => (None, Some(subscription.phid.clone())),
        None => (None, None),
    };
    let applied_phids: Vec<String> = first
        .into_iter()
        .chain(review.iter().map(|(_, phid, _)| phid.clone()))
        .chain(author_action.iter().map(|(_, phid)| phid.clone()))
        .chain(void.iter().map(|(phid, _, _)| phid.clone()))
        .chain(title.iter().map(|(phid, _)| phid.clone()))
        .chain(late)
        .chain(object_mention.iter().map(|mention| mention.phid.clone()))
        .chain(comment.iter().map(|(_, phid, _)| phid.clone()))
        .chain(status.iter().map(|(phid, _)| phid.clone()))
        .collect();
    let published = publication(database, &revision, &facts, &actor, &applied_phids).await?;
    let object_mention = match object_mention {
        Some(mention) => {
            let data = mention_task(database, &mention, &actor, &published.must_encrypt).await?;
            Some((mention, data))
        }
        None => None,
    };
    Ok(Plan {
        actor,
        group_id: random::random_characters(32),
        subscription,
        review,
        author_action,
        void,
        title,
        object_mention,
        comment,
        status,
        applied_phids,
        rules: published.rules,
        transcript: published.transcript,
        herald_header: published.herald_header,
        worker_data: published.worker_data,
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
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
         commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
         dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, NULL, 0, ?, ?, ?, ?, ?, ?, ?)",
        database.table("differential", "differential_transaction")
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
    if let Some(subscription) = &plan.subscription {
        write_subscription(request, plan, subscription, &metadata, connection).await?;
    }
    if let Some((review, phid, updates)) = &plan.review {
        let row = (
            phid.as_str(),
            review.transaction_type(),
            "false",
            "true",
            &*metadata,
        );
        insert_transaction(request, plan, connection, row).await?;
        write_reviewers(request, plan, *review, updates, connection).await?;
    }
    if let Some((action, phid)) = &plan.author_action {
        let row = (
            phid.as_str(),
            action.transaction_type(),
            "false",
            "true",
            &*metadata,
        );
        insert_transaction(request, plan, connection, row).await?;
    }
    if let Some((phid, old)) = &plan.title {
        let (old, new) = (
            php::json_encode(&json!(old)),
            php::json_encode(&json!(revision.title)),
        );
        let row = (
            phid.as_str(),
            "differential.revision.title",
            &*old,
            &*new,
            &*metadata,
        );
        insert_transaction(request, plan, connection, row).await?;
    }
    if let Some((phid, reviewers, force)) = &plan.void {
        write_void(request, plan, (phid, reviewers, force), connection).await?;
    }
    // The edge transaction is saved with the others, before the comment;
    // the edges and the inverse edit come with the external effects.
    if let Some((mention, _)) = &plan.object_mention {
        write_mention_transaction(request, plan, mention, connection).await?;
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
    if let Some((mention, _)) = &plan.object_mention {
        write_object_mention(request, plan, mention, connection).await?;
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
    write_herald(request, plan, connection).await?;
    queue_publishing(request, plan, connection).await?;
    transaction.commit().await?;
    Ok(())
}

/// The metadata of an edge transaction of the edit's group.
fn edge_metadata(plan: &Plan, edge_type: i64) -> String {
    php::json_encode(&json!({"edge:type": edge_type, "core.groupID": plan.group_id}))
}

/// The transaction of the "mentions" edge.
async fn write_mention_transaction(
    request: &ApiRequest<'_>,
    plan: &Plan,
    mention: &ObjectMention,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let mut new = mention.old.clone();
    new.push(mention.target.phid.as_str().to_owned());
    let (old, new) = (
        php::json_encode(&json!(mention.old)),
        php::json_encode(&json!(new)),
    );
    let metadata = edge_metadata(plan, edges::OBJECT_MENTIONS_OBJECT);
    let row = (mention.phid.as_str(), "core:edge", &*old, &*new, &*metadata);
    insert_transaction(request, plan, connection, row).await
}

/// The "mentions" edges, and the inverse edit of the mentioned revision:
/// its transaction and `dateModified`.
async fn write_object_mention(
    request: &ApiRequest<'_>,
    plan: &Plan,
    mention: &ObjectMention,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.phid.as_str();
    let target = mention.target.phid.as_str();
    for (src, edge_type, dst) in [
        (revision, edges::OBJECT_MENTIONS_OBJECT, target),
        (target, edges::OBJECT_MENTIONED_BY_OBJECT, revision),
    ] {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (src, type, dst, dateCreated, seq, dataID) VALUES (?, ?, ?, ?, 0, \
             NULL) ON DUPLICATE KEY UPDATE dataID = VALUES(dataID)",
            database.table("differential", "edge")
        ))
        .bind(src)
        .bind(edge_type)
        .bind(dst)
        .bind(request.now)
        .execute(&mut *connection)
        .await?;
    }
    // The inverse edit: the edges after the write, newest first.
    let mut after = vec![revision.to_owned()];
    after.extend(mention.inverse_old.iter().cloned());
    let (old, new) = (
        php::json_encode(&json!(mention.inverse_old)),
        php::json_encode(&json!(after)),
    );
    let inverse = edge_metadata(plan, edges::OBJECT_MENTIONED_BY_OBJECT);
    let row = (
        mention.inverse_phid.as_str(),
        "core:edge",
        &*old,
        &*new,
        &*inverse,
    );
    insert_transaction_on(request, plan, target, connection, row).await?;
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET dateModified = ? WHERE id = ?",
        database.table("differential", "differential_revision")
    ))
    .bind(request.now)
    .bind(mention.target.id)
    .execute(&mut *connection)
    .await?;
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
    if let Some((mention, data)) = &plan.object_mention {
        let target = mention.target.phid.as_str();
        worker::schedule_task(connection, database, publish(data, target), request.now).await?;
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

/// The `core:subscribers` transaction of `applyImplicitCC()` and its edges
/// (`PhabricatorEdgeEditor`, with the inverse edge).
async fn write_subscription(
    request: &ApiRequest<'_>,
    plan: &Plan,
    subscription: &Subscription,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.phid.as_str();
    let (phid, old) = (&subscription.phid, &subscription.old);
    let mut new = old.clone();
    new.extend(subscription.added.iter().cloned());
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
         commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
         dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, NULL, 0, 'core:subscribers', \
         ?, ?, ?, ?, ?, ?)",
        database.table("differential", "differential_transaction")
    ))
    .bind(phid)
    .bind(&plan.actor)
    .bind(revision)
    .bind(&plan.actor)
    .bind(php::json_encode(&json!(old)))
    .bind(php::json_encode(&json!(new)))
    .bind(CONDUIT_SOURCE)
    .bind(metadata)
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *connection)
    .await?;
    // PhabricatorEdgeEditor numbers the new edges of each source.
    for (seq, added) in (0_i64..).zip(&subscription.added) {
        for (application, src, edge_type, dst, seq) in [
            (
                "differential",
                revision,
                edges::OBJECT_HAS_SUBSCRIBER,
                added.as_str(),
                seq,
            ),
            (
                "user",
                added.as_str(),
                edges::SUBSCRIBED_TO_OBJECT,
                revision,
                0,
            ),
        ] {
            phab_db::sqlx::query(&format!(
                "INSERT INTO {} (src, type, dst, dateCreated, seq, dataID) VALUES (?, ?, ?, ?, ?, \
                 NULL) ON DUPLICATE KEY UPDATE dataID = VALUES(dataID)",
                database.table(application, "edge")
            ))
            .bind(src)
            .bind(edge_type)
            .bind(dst)
            .bind(request.now)
            .bind(seq)
            .execute(&mut *connection)
            .await?;
        }
    }
    Ok(())
}

/// The transcript, the rules' applied state and the saved header.
async fn write_herald(
    request: &ApiRequest<'_>,
    plan: &Plan,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = &plan.revision;
    let transcript = &plan.transcript;
    phab_db::sqlx::query(&format!(
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
    .await?;
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
    Ok(())
}

/// `differential.revision.edit`.
pub struct RevisionEdit;

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
            let plan = plan(request).await?;
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
    fn finds_references() {
        assert!(!has_references("Looks good to me."));
        assert!(!has_references("See D123."));
        assert!(has_references("See T123."));
        // Mentions are fine, numbers in usernames too.
        assert!(!has_references("Thanks @alice and @bob2"));
        assert!(has_references("Thanks @alice for F2"));
        assert!(has_references("{F1}"));
        assert!(has_references("https://example.com"));
    }

    #[test]
    fn rule_monograms_like_php() {
        assert_eq!(rule_monograms("<1>, <2>"), json!(["H1", "H2"]));
        assert_eq!(rule_monograms("none"), json!([]));
    }
}
