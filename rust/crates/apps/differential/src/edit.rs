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
//! when nothing else does (`applyImplicitCC()`), and so does the edit
//! form's empty comment, which then has no effect. A new diff ([`update`]),
//! alone or with a comment, a new title, summary, test plan, subscribers
//! or tags. Inline comments ([`inlines`]): the `inline` transactions, and
//! the actor's drafts the comment form publishes ([`apply_with_drafts()`]),
//! with any of these, and the "Done" states marked in a draft which the
//! first comment, inline comment or update publishes ([`inline_state`]).
//! "Close Revision" (`close`, which `differential.close` applies too). The
//! stored custom fields ([`custom_fields`]): Mozilla's Bugzilla bug ID and
//! uplift request, and the auditors. Transactions without effect are
//! dropped, as the Conduit editor continues on no effect, so arc's
//! `differential.updaterevision` (the diff and every field of the message)
//! is ported when the fields with an effect are.
//!
//! The daemons' edits, by the omnipotent viewer acting as an application:
//! the `harbormaster:buildable` transaction Harbormaster publishes
//! ([`publish_buildable()`]).
//!
//! Without `objectIdentifier`, a new revision (`core:create`): a diff, a
//! title, a summary and a test plan, reviewers, subscribers and tags,
//! whose texts have no remarkup effect (mentions, references, files and
//! dependencies of new revisions are left to PHP). New revisions are
//! drafts which don't broadcast, and this fork doesn't promote drafts in
//! the editor (moz-phab requests review): Herald's mail actions are
//! forbidden on drafts (`setForbiddenAction()`), and their rules don't
//! match. Mozilla's extensions add no editor effect on creation: their
//! event listeners only render pages, and the Bugzilla bug ID and uplift
//! fields only act on their own transactions.

mod commits;
mod custom_fields;
mod dependencies;
mod files;
mod herald_actions;
mod herald_fields;
mod inline_state;
mod inlines;
mod lists;
mod merge_checks;
mod monograms;
mod reverts;
mod reviewers;
mod stack;
mod subscribers;
mod tags;
mod update;

use phab_app_herald::engine::{self, Adapter, Applied, Evaluation, Rule, Subject};
use phab_conduit::{ApiRequest, Context, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php, random};
use phab_db::{Database, edges, rows, worker};
use serde_json::{Map, Value, json};

use crate::search::{self, Constraints, Order, Revisions};
use crate::storage::Revision;

/// The content source of a Conduit call (`PhabricatorConduitContentSource`).
const CONDUIT_SOURCE: &str = r#"{"source":"conduit","params":[]}"#;

/// The content source of a web request (`PhabricatorWebContentSource`).
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

/// The content source of the daemons (`PhabricatorDaemonContentSource`).
const DAEMON_SOURCE: &str = r#"{"source":"daemon","params":[]}"#;

/// Where an edit comes from (`PhabricatorContentSource`): its transactions
/// and comments say so, Herald's "Content source" field and the `via` mail
/// stamp read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Source {
    /// `differential.revision.edit` (`PhabricatorConduitContentSource`).
    #[default]
    Conduit,
    /// A form of the web interface (`PhabricatorWebContentSource`).
    Web {
        /// The `remarkup.control` metadata the comment form gives its
        /// comment (`buildCommentResponse()`): the decoded
        /// `comment_metadata`, `null` without one.
        remarkup_control: Value,
    },
    /// A task of the daemons (`PhabricatorDaemonContentSource`), whose
    /// omnipotent actor acts as an application or a user.
    Daemon,
    /// PHP's edit form (`PhabricatorEditEngine::buildEditResponse()`): a
    /// web content source, an editor which continues on no effect
    /// (`setContinueOnNoEffect(true)`), and remarkup fields whose
    /// transactions carry the form's empty `remarkup.control` metadata
    /// (`PhabricatorRemarkupEditField::getMetadata()`).
    EditForm,
}

impl Source {
    /// The serialized content source of the rows.
    fn serialized(&self) -> &'static str {
        match self {
            Self::Conduit => CONDUIT_SOURCE,
            Self::Web { .. } | Self::EditForm => WEB_SOURCE,
            Self::Daemon => DAEMON_SOURCE,
        }
    }

    /// `getSourceTypeConstant()`.
    fn constant(&self) -> &'static str {
        match self {
            Self::Conduit => "conduit",
            Self::Web { .. } | Self::EditForm => "web",
            Self::Daemon => "daemon",
        }
    }

    /// The metadata the comment transaction starts with.
    fn comment_metadata(&self) -> Value {
        match self {
            Self::Conduit | Self::Daemon | Self::EditForm => json!({}),
            Self::Web { remarkup_control } => json!({"remarkup.control": remarkup_control}),
        }
    }
}

/// `PhabricatorProjectObjectHasWatcherEdgeType`.
const PROJECT_HAS_WATCHER: i64 = 47;

/// `PhabricatorProjectProjectHasObjectEdgeType`.
const PROJECT_HAS_OBJECT: i64 = 42;

/// A transaction without effect, which the editor drops as it continues on
/// no effect (`setContinueOnNoEffect()`); an edit of nothing else is left
/// to PHP, which still saves and publishes the revision.
const NO_EFFECT: &str = "edits without effect";

/// `DifferentialHeraldStateReasons::REASON_DRAFT`.
const DRAFT_REASON: &str = "differential.draft";

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
    Text(TextField),
    Reviewers,
    Subscribers,
    Projects,
    Stack,
    Update,
    Close,
    Buildable,
    Custom(custom_fields::Field),
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
    /// "Close Revision".
    close: bool,
    /// The buildable whose status Harbormaster publishes, and the status.
    buildable: Option<(String, Option<String>)>,
    /// The stored custom fields set.
    custom_fields: Vec<custom_fields::Wanted>,
    /// A new title.
    title: Option<String>,
    /// The new summary or test plan, or both for a new revision.
    texts: Vec<(TextField, String)>,
    /// The diff to attach (`update`).
    update: Option<String>,
    /// Reviewers added, removed or set.
    reviewers: Option<reviewers::Wanted>,
    /// Subscribers added, removed or set.
    subscribers: Option<lists::Wanted>,
    /// Tags added, removed or set.
    projects: Option<lists::Wanted>,
    /// Parent or child revisions added, removed or set, with the edge type.
    stack: Option<(i64, lists::Wanted)>,
    /// The `inline` transactions.
    inlines: Vec<inlines::Wanted>,
    /// How many inline comments come before the comment, for the order of
    /// their texts.
    comment_position: usize,
    /// The comment form's empty comment, which it only sends when nothing
    /// else is published.
    empty_comment: bool,
    /// The edit form's empty comment field (`update.comment`): a comment
    /// transaction without effect, which PHP drops, but only after it
    /// implied the actor's subscription (`applyImplicitCC()`).
    form_comment: bool,
    /// The texts of the inline comments the edit publishes, once planned.
    inline_texts: Vec<String>,
    /// How many actions come before the first comment, inline comment or
    /// diff update (that one included), which the inline state follows.
    state_trigger: Option<usize>,
    /// Whether transactions without effect are dropped
    /// (`setContinueOnNoEffect()`), once planned.
    continues: bool,
}

/// The ported transactions of the `transactions` parameter: a comment, a
/// review, or both. `is_new` when they create a revision; `drafts` when
/// the edit publishes the actor's draft inline comments, which make an
/// empty comment unnecessary.
fn wanted(
    request: &ApiRequest<'_>,
    (is_new, drafts): (bool, bool),
    source: &Source,
) -> Result<Wanted, MethodError> {
    let Some(Value::Array(transactions)) = request.value("transactions") else {
        return Err(MethodError::Unsupported("unusual transaction lists"));
    };
    if is_new && drafts {
        return Err(MethodError::Unsupported("creations not ported"));
    }
    let mut wanted = Wanted::default();
    for transaction in transactions {
        match (
            transaction.get("type").and_then(Value::as_str),
            transaction.get("value"),
        ) {
            (Some("comment"), Some(Value::String(text)))
                if wanted.comment.is_none() && !wanted.empty_comment && !wanted.form_comment =>
            {
                if drafts && text.is_empty() {
                    wanted.empty_comment = true;
                    continue;
                }
                if *source == Source::EditForm && !is_new && text.is_empty() {
                    wanted.form_comment = true;
                    continue;
                }
                if is_empty_comment(text) {
                    return Err(MethodError::Unsupported("empty comments"));
                }
                wanted.comment = Some(text.clone());
                wanted.comment_position = wanted.inlines.len();
                wanted.state_trigger.get_or_insert(wanted.order.len());
            }
            (Some("inline"), Some(value)) if !drafts => {
                wanted.inlines.push(inlines::parse(value)?);
                wanted.state_trigger.get_or_insert(wanted.order.len());
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
            (Some("close"), Some(Value::Bool(true))) if !wanted.close => {
                wanted.close = true;
                wanted.order.push(Action::Close);
            }
            (Some("title"), Some(Value::String(title))) if wanted.title.is_none() => {
                wanted.title = Some(title.clone());
                wanted.order.push(Action::Title);
            }
            (Some(kind @ ("summary" | "testPlan")), Some(Value::String(text))) => {
                let field = if kind == "summary" {
                    TextField::Summary
                } else {
                    TextField::TestPlan
                };
                // One of each; an existing revision gets one with effect.
                if wanted.texts.iter().any(|(known, _)| *known == field) {
                    return Err(MethodError::Unsupported("transactions not ported"));
                }
                wanted.texts.push((field, text.clone()));
                wanted.order.push(Action::Text(field));
            }
            (Some("update"), Some(value)) if wanted.update.is_none() => {
                wanted.update = Some(update::parse(value)?);
                wanted.order.push(Action::Update);
                wanted.state_trigger.get_or_insert(wanted.order.len());
            }
            (Some(kind), Some(value)) => wanted_field(&mut wanted, kind, value)?,
            _ => return Err(MethodError::Unsupported("transactions not ported")),
        }
    }
    if wanted == Wanted::default() {
        return Err(MethodError::Unsupported("empty transaction lists"));
    }
    // The comment form's inline state follows its actions.
    if drafts {
        wanted.state_trigger = Some(wanted.order.len());
    }
    if is_new {
        check_creation(&wanted)?;
    }
    check_combination(&wanted, is_new)?;
    Ok(wanted)
}

/// The transactions of a new revision: a diff and a title, which PHP
/// requires, and lists or texts. Comments, actions and stack edits of new
/// revisions are left to PHP.
fn check_creation(wanted: &Wanted) -> Result<(), MethodError> {
    if wanted.update.is_none() || wanted.title.is_none() {
        // "You must specify an initial diff", "Revisions must have a title".
        return Err(MethodError::Unsupported("creations PHP refuses"));
    }
    let ported = |action: &Action| {
        matches!(
            action,
            Action::Update
                | Action::Title
                | Action::Text(_)
                | Action::Reviewers
                | Action::Subscribers
                | Action::Projects
                | Action::Custom(_)
        )
    };
    // Inline comments "can not be added while creating a revision".
    if wanted.comment.is_some() || !wanted.inlines.is_empty() || !wanted.order.iter().all(ported) {
        return Err(MethodError::Unsupported("creations not ported"));
    }
    Ok(())
}

/// Several actions, but those whose combination isn't ported: stack edits
/// (written with the links), "Request Review" (whose void comes later),
/// reviews or author actions with a reviewer edit (both change reviewers),
/// and closing with another action on the review.
fn check_combination(wanted: &Wanted, is_new: bool) -> Result<(), MethodError> {
    if wanted.order.len() < 2 {
        return Ok(());
    }
    let has = |action| wanted.order.contains(&action);
    let reviews = has(Action::Review) || has(Action::Author) || has(Action::Reviewers);
    if has(Action::Close) && (reviews || has(Action::Update)) {
        return Err(MethodError::Unsupported("several actions"));
    }
    // A new diff voids reviews, which actions on reviewers would see; a
    // reviewer edit without effect is dropped, which is checked once
    // planned.
    let changes_reviewers = has(Action::Review) || has(Action::Author);
    if has(Action::Stack)
        || wanted.author_action == Some(AuthorAction::RequestReview)
        || (has(Action::Reviewers) && (has(Action::Review) || has(Action::Author)))
        || (has(Action::Update) && changes_reviewers && !is_new)
    {
        return Err(MethodError::Unsupported("several actions"));
    }
    Ok(())
}

/// A custom field, once each, or a list transaction.
fn wanted_field(wanted: &mut Wanted, kind: &str, value: &Value) -> Result<(), MethodError> {
    let Some(field) = custom_fields::Field::of_conduit_key(kind) else {
        return wanted_list(wanted, kind, value);
    };
    if wanted.order.contains(&Action::Custom(field)) {
        return Err(MethodError::Unsupported("transactions not ported"));
    }
    wanted
        .custom_fields
        .push(custom_fields::parse(field, value)?);
    wanted.order.push(Action::Custom(field));
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

/// Whether the call creates a revision: it names none.
fn is_creation(request: &ApiRequest<'_>) -> Result<bool, MethodError> {
    match request.value("objectIdentifier") {
        None | Some(Value::Null) => Ok(true),
        // `if ($identifier)`: other falsy values create too.
        Some(value) if !php::is_truthy(value) => Err(MethodError::Unsupported(
            "creations and unusual identifiers",
        )),
        Some(_) => Ok(false),
    }
}

/// `initializeNewRevision()`: a draft of the actor, which doesn't
/// broadcast, with the application's default view policy for both
/// policies. Its ID is known once saved.
fn new_revision(request: &ApiRequest<'_>, actor: &str) -> (Revision, Facts) {
    let policies = phab_policy::ApplicationPolicies::from_config(&request.context.config);
    let policy = policies
        .custom_policy(
            phab_policy::application::DIFFERENTIAL,
            phab_policy::Capability("differential.default.view"),
            phab_policy::POLICY_USER,
        )
        .to_owned();
    let revision = Revision {
        id: 0,
        phid: Phid::generate(PhidType::REVISION, None),
        title: String::new(),
        status: "draft".to_owned(),
        author_phid: Some(Phid::new(actor)),
        view_policy: policy.clone(),
        edit_policy: policy,
        repository_phid: None,
        active_diff_phid: None,
        date_modified: request.now,
        reviewers: Vec::new(),
    };
    let facts = Facts {
        mentioned: json!([]),
        should_broadcast: false,
        reviewers: Vec::new(),
        projects: Vec::new(),
        subscribers: Vec::new(),
        muted: Vec::new(),
        old_recipients: (Vec::new(), Vec::new()),
        properties: Some(Map::new()),
    };
    (revision, facts)
}

/// `PhabricatorPolicyFilter::hasCapability()` of `CAN_EDIT`; nothing is
/// required to edit a new revision (`applyCapabilityChecks()`).
async fn can_edit(request: &ApiRequest<'_>, revision: &Revision) -> Result<bool, MethodError> {
    if revision.id == 0 {
        return Ok(true);
    }
    let context = request.context;
    crate::storage::can_edit(
        &context.database,
        &context.config,
        &request.viewer,
        revision,
        request.now,
    )
    .await
    .map_err(MethodError::internal)
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
    /// The `properties`, as PHP decodes them; `None` when they are not a
    /// dictionary, which edits setting a property leave to PHP.
    properties: Option<Map<String, Value>>,
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
        let properties = match php::round_trip(&rows::json(&row, "properties")?) {
            Value::Object(properties) => Some(properties),
            Value::Array(list) if list.is_empty() => Some(Map::new()),
            _ => None,
        };
        // getShouldBroadcast(): the "draft.broadcast" property, true when
        // missing.
        let should_broadcast = properties
            .as_ref()
            .and_then(|properties| properties.get("draft.broadcast"))
            .is_none_or(php::is_truthy);
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
            properties,
        };
        facts.old_recipients = mail_recipients(revision, &facts);
        Ok(facts)
    }
}

/// Edits PHP's editor makes which Rust leaves to it: the message Herald's
/// run sends to the automatic buildables of the active diff which are
/// still preparing builds (`applyHeraldRules()`), and mail to project
/// watchers.
async fn check_side_effects(
    request: &ApiRequest<'_>,
    revision: &Revision,
    facts: &Facts,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    if let Some(diff) = &revision.active_diff_phid {
        let sql = format!(
            "SELECT 1 FROM {} WHERE buildablePHID = ? AND isManualBuildable = 0 AND \
             buildableStatus = 'preparing' LIMIT 1",
            database.table("harbormaster", "harbormaster_buildable")
        );
        if phab_db::sqlx::query(&sql)
            .bind(diff.as_str())
            .fetch_optional(database.pool())
            .await?
            .is_some()
        {
            return Err(MethodError::Unsupported(
                "revisions with buildables preparing builds",
            ));
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
    (wanted, old_texts): (&Wanted, &[(TextField, String)]),
    actor: &str,
) -> Result<Option<Subscription>, MethodError> {
    let context = request.context;
    let database = &context.database;
    // shouldImplyCC(): only comments, inline ones too, subscribe their
    // author.
    let commented =
        wanted.comment.is_some() || wanted.form_comment || !wanted.inline_texts.is_empty();
    let implicit = commented && implies_subscription(database, revision, facts, actor).await?;
    // Only new mentions count: text a field had mentioned no one anew.
    let mut mentioned: Vec<(String, bool, bool, bool)> = Vec::new();
    for text in new_texts(wanted) {
        for user in mentioned_users(request, text).await? {
            if !mentioned.iter().any(|known| known.0 == user.0) {
                mentioned.push(user);
            }
        }
    }
    for (_, old) in old_texts {
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
    if added.iter().any(|phid| phid != actor) && !can_edit(request, revision).await? {
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
    /// The summary and test plan transactions: the field, its PHID, the
    /// old and the new text.
    texts: Vec<(TextField, String, String, String)>,
    /// The `differential:update` transaction and its void.
    update: Option<update::Planned>,
    /// The `differential.revision.reviewers` transaction.
    reviewer_edit: Option<reviewers::Planned>,
    /// The `core:subscribers` transaction of `subscribers.*`.
    subscriber_edit: Option<Subscription>,
    /// The `core:edge` transaction of `projects.*`.
    tag_edit: Option<tags::Planned>,
    /// The `differential.revision.void` transaction of "Request Review":
    /// its PHID, the reviewers it voids and the statuses it must void.
    void: Option<PlannedVoid>,
    /// The `differential.revision.close` transaction.
    close: Option<String>,
    /// The `harbormaster:buildable` transaction.
    buildable: Option<BuildableStatus>,
    /// The `core:inlinestate` transaction, and how many actions precede it.
    inline_state: Option<(usize, inline_state::Planned)>,
    /// The `core:customfield` transactions with an effect.
    custom_fields: Vec<custom_fields::Planned>,
}

/// The `harbormaster:buildable` transaction
/// (`DifferentialRevisionBuildableTransaction`).
struct BuildableStatus {
    phid: String,
    buildable_phid: String,
    /// The revision's status of the buildable before, and after.
    old: Option<String>,
    new: Option<String>,
}

impl Actions {
    /// The transactions of the actions, in the order PHP applies them: an
    /// author action's void follows it.
    fn phids(&self) -> Vec<String> {
        let mut phids = Vec::new();
        for (index, action) in self.order.iter().enumerate() {
            if let Some((_, state)) = self.inline_state.as_ref().filter(|(at, _)| *at == index) {
                phids.push(&state.phid);
            }
            match action {
                Action::Review => phids.extend(self.review.iter().map(|(_, phid, _)| phid)),
                Action::Author => {
                    phids.extend(self.author_action.iter().map(|(_, phid)| phid));
                    phids.extend(self.void.iter().map(|(phid, _, _)| phid));
                }
                Action::Title => phids.extend(self.title.iter().map(|(phid, _)| phid)),
                Action::Text(field) => phids.extend(
                    self.texts
                        .iter()
                        .filter(|(known, ..)| *known == *field)
                        .map(|(_, phid, ..)| phid),
                ),
                Action::Update => {
                    if let Some(update) = &self.update {
                        phids.push(&update.phid);
                        phids.extend(update.void.iter().map(|(phid, _, _)| phid));
                    }
                }
                Action::Reviewers => phids.extend(self.reviewer_edit.iter().map(|edit| &edit.phid)),
                Action::Subscribers => {
                    phids.extend(self.subscriber_edit.iter().map(|edit| &edit.phid));
                }
                Action::Projects => phids.extend(self.tag_edit.iter().map(|edit| &edit.phid)),
                Action::Close => phids.extend(self.close.iter()),
                Action::Buildable => phids.extend(self.buildable.iter().map(|edit| &edit.phid)),
                Action::Custom(field) => phids.extend(
                    self.custom_fields
                        .iter()
                        .filter(|edit| edit.field() == *field)
                        .map(|edit| &edit.phid),
                ),
                Action::Stack => {}
            }
        }
        let state = self.inline_state.as_ref();
        if let Some((_, state)) = state.filter(|(at, _)| *at >= self.order.len()) {
            phids.push(&state.phid);
        }
        phids.into_iter().cloned().collect()
    }
}

/// A merge conflict check an edit queues.
enum MergeCheck {
    /// `queueChecks()`: the revision and those stacked above it.
    Stack(String),
    /// `queueCheck()` of the revision's new diff, then
    /// `queueDescendantChecks()`.
    Diff(String, String),
}

/// The planned edit.
struct Plan {
    /// Where the edit comes from.
    source: Source,
    /// The `editPolicy` of the transactions (`populateTransaction()`): the
    /// actor's PHID, `no-one` for the omnipotent actor.
    edit_policy: String,
    /// The revision's `properties` once the edit set some, encoded.
    properties: Option<String>,
    /// The revision as the edit leaves it; a new one has no ID until
    /// it is saved.
    revision: Revision,
    /// The `core:create` transaction of a new revision.
    create: Option<String>,
    actor: String,
    /// The `core:subscribers` transaction `applyImplicitCC()` adds: its
    /// PHID and the subscribers before.
    subscription: Option<Subscription>,
    actions: Actions,
    /// The comment: its text, transaction and comment PHIDs.
    comment: Option<(String, String, String)>,
    /// The inline comments, after the comment.
    inlines: inlines::Inlines,
    /// The `differential.revision.status` transaction `updateReviewStatus()`
    /// adds, with the old status.
    status: Option<(String, String)>,
    /// The revisions the edit links to, dependencies then mentions, and
    /// the publish tasks of their targets.
    links: Vec<(Link, Vec<String>)>,
    /// The implicit `core:file` transaction of the files the texts
    /// reference.
    file_edit: Option<files::Planned>,
    /// The merge conflict checks: the linked revisions' sub-editors', then
    /// the edit's.
    merge_checks: Vec<MergeCheck>,
    /// The Herald editor's transactions.
    herald: herald_actions::Edits,
    /// The IDs of the rules to mark as applied to the revision.
    herald_remembered: Vec<String>,
    /// Every transaction, in the order PHP applies them.
    applied_phids: Vec<String>,
    group_id: String,
    rules: Vec<Rule>,
    transcript: engine::TranscriptRow,
    herald_header: String,
    /// The publish task's data, whose monogram stamp a new revision gets
    /// once saved.
    worker_data: Value,
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
    let mut rules = rules;
    // needAppliedToPHIDs(): where the rules applied before.
    if !rules.is_empty() {
        let sql = format!(
            "SELECT ruleID FROM {} WHERE phid = ? AND ruleID IN ({})",
            database.table("herald", "herald_ruleapplied"),
            phab_db::placeholders(rules.len())
        );
        let query = phab_db::sqlx::query(&sql).bind(revision.phid.as_str());
        let applied: Vec<String> =
            phab_db::bind_all(query, rules.iter().map(|rule| rule.id.as_str()))
                .fetch_all(database.pool())
                .await?
                .iter()
                .map(|row| rows::int(row, "ruleID").map(|id| id.to_string()))
                .collect::<Result<_, _>>()?;
        for rule in &mut rules {
            rule.applied_before = applied.contains(&rule.id);
        }
    }
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
    // Drafts forbid the mailable state (`setForbiddenAction()`), which the
    // email actions require: those rules don't match.
    let forbidden: &[(&str, &str)] = if facts.should_broadcast {
        &[]
    } else {
        &[("email.other", DRAFT_REASON), ("email.self", DRAFT_REASON)]
    };
    let evaluation = engine::evaluate_forbidding(&rules, &subject, &values, forbidden);
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

/// The `state` of the publish task (`getWorkerState()`); `is_new` for a
/// revision the edit creates.
fn worker_state(
    revision: &Revision,
    facts: &Facts,
    (actor, is_new, via): (&str, bool, &str),
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
        stamp("string", "via", json!(via)),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!(must_encrypt)),
        stamp("bool", "new", json!(is_new)),
        stamp("phid", "mention", facts.mentioned.clone()),
        stamp("string", "herald", rule_monograms(herald_header)),
        stamp("phid", "removed", json!(removed)),
        stamp("phid", "tag", json!(facts.projects)),
        stamp("phid", "subscriber", json!(facts.subscribers)),
    ];
    json!({
        "parentMessageID": null,
        "isNewObject": is_new,
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

/// Sets the `monogram` stamp of the publish task's data of a revision
/// saved with `id`.
fn set_monogram(data: &mut Value, id: i64) {
    let stamps = data["state"]["mailStamps"].as_array_mut();
    for stamp in stamps.into_iter().flatten() {
        if stamp["key"] == "monogram" {
            stamp["value"] = json!(format!("D{id}"));
        }
    }
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

/// `DifferentialRevisionCloseTransaction`, once PHP's validation passes
/// (unless a commit closes the revision, whatever its state): it publishes
/// the revision, records whether it was accepted, and makes it broadcast
/// (T13300). Returns the properties it sets. Closing a draft broadcasts it
/// for the first time, which is left to PHP.
async fn plan_close(
    request: &ApiRequest<'_>,
    revision: &mut Revision,
    facts: &Facts,
    (actor, by_commit): (&str, bool),
) -> Result<Vec<(String, Value)>, MethodError> {
    let is_author = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor);
    let allowed = is_author
        || request
            .context
            .config
            .value("differential.always-allow-close")
            .is_some_and(php::is_truthy);
    let was_accepted = revision.status == "accepted";
    if !by_commit && (revision.is_closed() || !was_accepted || !allowed) {
        return Err(MethodError::Unsupported("closes PHP refuses"));
    }
    if !facts.should_broadcast {
        return Err(MethodError::Unsupported("closes of drafts"));
    }
    if !can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    "published".clone_into(&mut revision.status);
    Ok(vec![
        ("wasAcceptedBeforeClose".to_owned(), json!(was_accepted)),
        ("draft.broadcast".to_owned(), json!(true)),
    ])
}

/// The revision's `properties` as the edit leaves them, encoded, if it
/// sets any: those of the new diff, then `edits` in order (a key keeps its
/// place).
fn final_properties(
    facts: &Facts,
    update: Option<&update::Planned>,
    edits: Vec<(String, Value)>,
) -> Result<Option<String>, MethodError> {
    let mut map = match (update, &facts.properties) {
        (Some(update), _) => update.properties.clone(),
        (None, _) if edits.is_empty() => return Ok(None),
        (None, Some(properties)) => properties.clone(),
        (None, None) => return Err(MethodError::Unsupported("unusual revision properties")),
    };
    for (key, value) in edits {
        map.insert(key, value);
    }
    crate::diff_editor::encode(&Value::Object(map)).map(Some)
}

/// The `differential.revision.void` transaction "Request Review" and new
/// diffs add (`expandTransaction()`): the reviewers whose accepts and
/// rejections it voids, and the statuses it must void (`force`, else
/// sticky accepts stay). Voids `revision`'s reviewers.
async fn plan_void(
    request: &ApiRequest<'_>,
    revision: &mut Revision,
    force: Vec<&'static str>,
    actor: &str,
) -> Result<Option<(Voided, Vec<&'static str>)>, MethodError> {
    let database = &request.context.database;
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

/// The byte length of the `title` column (`text255`).
const TITLE_LENGTH: usize = 255;

/// `DifferentialRevisionTitleTransaction`: the new title, which needs
/// `CAN_EDIT` (PHP raises a policy exception otherwise). Returns the old
/// title. An unchanged title has no effect, yet PHP still saves and
/// publishes the revision; that is left to it, as are the empty and
/// overlong titles PHP refuses.
async fn plan_title(
    request: &ApiRequest<'_>,
    revision: &mut Revision,
    title: String,
) -> Result<String, MethodError> {
    if title == revision.title {
        return Err(MethodError::Unsupported(NO_EFFECT));
    }
    if title.is_empty() || title.len() > TITLE_LENGTH {
        return Err(MethodError::Unsupported("titles PHP refuses"));
    }
    if !can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    Ok(std::mem::replace(&mut revision.title, title))
}

/// The new remarkup texts of an edit: the fields', then the comment and
/// the inline comments, in the order of their transactions.
fn new_texts(wanted: &Wanted) -> Vec<&str> {
    let (before, after) = wanted.inline_texts.split_at(wanted.comment_position);
    wanted
        .texts
        .iter()
        .map(|(_, text)| text.as_str())
        .chain(before.iter().map(String::as_str))
        .chain(wanted.comment.as_deref())
        .chain(after.iter().map(String::as_str))
        .collect()
}

/// A new summary or test plan, which needs `CAN_EDIT`: the old text, empty
/// for a new revision. An unchanged text is left to PHP, as an unchanged
/// title is, and so are texts PHP can't render unambiguously in a commit
/// message (`validateMessageCorpus()`).
async fn plan_text(
    request: &ApiRequest<'_>,
    revision: &Revision,
    field: TextField,
    new: &str,
) -> Result<String, MethodError> {
    let context = request.context;
    let database = &context.database;
    if crate::commit_message::parse::names_fields(&context.config, new)? {
        return Err(MethodError::Unsupported(
            "texts which read as commit message fields",
        ));
    }
    let old = if revision.id == 0 {
        String::new()
    } else {
        let sql = format!(
            "SELECT {} AS text FROM {} WHERE id = ?",
            field.column(),
            database.table("differential", "differential_revision")
        );
        rows::text(
            &phab_db::sqlx::query(&sql)
                .bind(revision.id)
                .fetch_one(database.pool())
                .await?,
            "text",
        )?
    };
    if old == new {
        return Err(MethodError::Unsupported(NO_EFFECT));
    }
    if !can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    Ok(old)
}

/// The texts of a new revision, whose remarkup must have no effect:
/// mentions, references and files of new revisions are left to PHP, and so
/// is a missing test plan, which PHP requires by default
/// (`differential.require-test-plan-field`).
fn check_new_texts(request: &ApiRequest<'_>, wanted: &Wanted) -> Result<(), MethodError> {
    let config = &request.context.config;
    let required = config
        .value("differential.require-test-plan-field")
        .is_none_or(php::is_truthy);
    let has_plan = wanted
        .texts
        .iter()
        .any(|(field, text)| *field == TextField::TestPlan && !text.is_empty());
    if required && !has_plan {
        return Err(MethodError::Unsupported("creations PHP refuses"));
    }
    for (_, text) in &wanted.texts {
        if !is_inert(request, text) {
            return Err(MethodError::Unsupported(
                "remarkup with effects in new revisions",
            ));
        }
    }
    Ok(())
}

/// Whether remarkup has no effect on an edit: no mentions, references,
/// files, dependencies or reverts.
fn is_inert(request: &ApiRequest<'_>, text: &str) -> bool {
    let config = &request.context.config;
    let engine = phab_remarkup::phabricator::engine(crate::markup::engine_config(config));
    let prepared = engine.prepare(text.as_bytes(), phab_remarkup::Mode::Html);
    prepared.mentioned_usernames().is_empty()
        && prepared.object_references().is_empty()
        && prepared.file_attach_intent_ids().is_empty()
        && dependencies::depends_on(text).is_empty()
        && monograms::REVERTS.parse(text).is_empty()
}

/// Drops a transaction without effect (`filterTransactions()`): its
/// capability is checked first all the same (`applyCapabilityChecks()`).
async fn drop_no_effect<T>(
    (request, continues): (&ApiRequest<'_>, bool),
    revision: &Revision,
    planned: Result<T, MethodError>,
) -> Result<Option<T>, MethodError> {
    match planned {
        Ok(planned) => Ok(Some(planned)),
        Err(MethodError::Unsupported(NO_EFFECT)) if continues => {
            if !can_edit(request, revision).await? {
                return Err(MethodError::Unsupported("edits the actor may not make"));
            }
            Ok(None)
        }
        Err(error) => Err(error),
    }
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
    /// The IDs of the rules to mark as applied to the revision.
    herald_remembered: Vec<String>,
    rules: Vec<Rule>,
    transcript: engine::TranscriptRow,
    herald_header: String,
    worker_data: Value,
}

/// What Herald's fields read of the edit: the actor, whether it creates
/// the revision, the summary or test plan set, and whether the edit
/// unsubscribes anyone.
fn herald_input<'a>(
    (actor, is_new, source): (&'a str, bool, &Source),
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
        is_new,
        source: source.constant(),
        texts: actions
            .texts
            .iter()
            .map(|(field, _, _, new)| (*field, new.as_str()))
            .collect(),
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
    );
    let mut published: Vec<String> = applied_phids.to_vec();
    published.extend(herald.phids());
    let mut state = worker_state(
        revision,
        facts,
        (input.actor, input.is_new, input.source),
        &outcome.must_encrypt,
        &herald_header,
    );
    state["heraldEmailPHIDs"] = json!(herald.email);
    state["heraldForcedEmailPHIDs"] = json!(herald.forced_email);
    let worker_data = json!({
        "objectPHID": revision.phid.as_str(),
        "actorPHID": input.actor,
        "xactionPHIDs": published,
        "state": state,
    });
    // Rules repeating the first time or on change which acted, and those on
    // change which matched again.
    let remembers = |rule_id: &String| {
        outcome.rules.iter().any(|rule| {
            rule.id == *rule_id && matches!(rule.repetition_policy.as_str(), "first" | "change")
        })
    };
    let mut herald_remembered: Vec<String> = outcome
        .applied
        .iter()
        .map(|applied| applied.effect.rule_id.clone())
        .filter(|rule_id| remembers(rule_id))
        .collect();
    herald_remembered.extend(outcome.evaluation.skipped.iter().cloned());
    Ok(Publication {
        must_encrypt: outcome.must_encrypt,
        herald,
        herald_remembered,
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
    (actor, via): (&str, &str),
    must_encrypt: &[String],
) -> Result<String, MethodError> {
    // Sub-editors run no Herald: they load the saved header, null when the
    // object has none.
    let saved = saved_header(database, &Phid::new(target.phid())).await?;
    let header = saved.as_deref().unwrap_or_default();
    let mut state = match &target.object {
        Linked::Revision { revision, facts } => {
            worker_state(revision, facts, (actor, false, via), must_encrypt, header)
        }
        Linked::Commit(commit) => commits::worker_state(commit, (actor, via), must_encrypt, header),
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
    (actor, via): (&str, &str),
    must_encrypt: &[String],
) -> Result<Vec<(Link, Vec<String>)>, MethodError> {
    let mut tasks = Vec::new();
    for link in links {
        let mut data = Vec::new();
        for target in &link.targets {
            data.push(link_task(database, target, (actor, via), must_encrypt).await?);
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
        // "Changes Planned" then "Request Review" keeps sticky accepts.
        let force = if old_status == "changes-planned" {
            Vec::new()
        } else {
            vec!["accepted"]
        };
        plan_void(request, revision, force, actor)
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
    Vec<(TextField, String, String, String)>,
);

/// The new title and the new summary and test plan, with their
/// transactions.
async fn plan_fields(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &mut Revision,
    old_texts: &[(TextField, String)],
) -> Result<FieldEdits, MethodError> {
    let texts = wanted
        .texts
        .iter()
        .map(|(field, new)| {
            let old = old_texts
                .iter()
                .find(|(known, _)| known == field)
                .map(|(_, old)| old.clone())
                .unwrap_or_default();
            (*field, transaction_phid(), old, new.clone())
        })
        .collect();
    let title = match wanted.title.clone() {
        Some(title) => {
            let planned = plan_title(request, revision, title).await;
            drop_no_effect((request, wanted.continues), revision, planned)
                .await?
                .map(|old| (transaction_phid(), old))
        }
        None => None,
    };
    Ok((title, texts))
}

/// The `update` transaction: its effects on the revision, and the void of
/// the reviews of an existing revision (`expandTransaction()`: rejections,
/// and accepts which aren't sticky).
async fn plan_update(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &mut Revision,
    facts: &Facts,
    actor: &str,
) -> Result<Option<update::Planned>, MethodError> {
    let Some(diff) = &wanted.update else {
        return Ok(None);
    };
    let is_new = revision.id == 0;
    let void = if is_new {
        None
    } else {
        plan_void(request, revision, Vec::new(), actor)
            .await?
            .map(|(reviewers, force)| (transaction_phid(), reviewers, force))
    };
    let mut planned = update::plan(request, diff, revision, facts, is_new).await?;
    planned.void = void;
    Ok(Some(planned))
}

/// The acting user, or what the omnipotent viewer acts as, and the
/// `editPolicy` of the transactions.
fn actor_phid(
    request: &ApiRequest<'_>,
    acting_as: Option<String>,
) -> Result<(String, String), MethodError> {
    if request.viewer.is_omnipotent {
        let phid = acting_as.ok_or(MethodError::Unsupported("edits by the omnipotent user"))?;
        return Ok((phid, phab_policy::POLICY_NOONE.to_owned()));
    }
    let phid = request.viewer.phid.as_ref();
    let phid = phid.ok_or(MethodError::Unsupported("edits by the omnipotent user"))?;
    Ok((phid.as_str().to_owned(), phid.as_str().to_owned()))
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
    // Several texts are those of a new revision, which have no effect.
    let (dependency, mut unmentionable) = match wanted.texts.first() {
        Some((_, text)) => dependencies::plan(request, revision, text).await?,
        None => (None, Vec::new()),
    };
    let (revert, reverted) = match wanted.texts.first() {
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
    (revision, facts): (&mut Revision, &mut Facts),
    actor: &str,
) -> Result<ListEdits, MethodError> {
    let planned = plan_reviewer_edit(request, wanted, revision, facts).await;
    let reviewer_edit = drop_no_effect((request, wanted.continues), revision, planned)
        .await?
        .flatten();
    let mut subscriber_edit = match &wanted.subscribers {
        Some(edit) => {
            let had_implied = subscription.is_some();
            let implied = subscription.take();
            let planned = subscribers::plan(request, edit, implied, revision, facts, actor).await;
            if had_implied && matches!(planned, Err(MethodError::Unsupported(NO_EFFECT))) {
                return Err(MethodError::Unsupported("several actions"));
            }
            drop_no_effect((request, wanted.continues), revision, planned).await?
        }
        None => None,
    };
    // An implicit CC merged with an explicit edit stays first.
    if subscriber_edit.as_ref().is_some_and(|edit| edit.first) {
        *subscription = subscriber_edit.take();
    }
    let tag_edit = match &wanted.projects {
        Some(edit) => {
            let planned = tags::plan(request, edit, revision, facts).await;
            drop_no_effect((request, wanted.continues), revision, planned).await?
        }
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
) -> Result<Vec<MergeCheck>, MethodError> {
    let mut checks = Vec::new();
    let rewiring = links
        .iter()
        .filter(|link| merge_checks::rewires_stack(link.edge_type));
    for link in rewiring.clone() {
        for target in &link.targets {
            if let Linked::Revision { revision, .. } = &target.object
                && merge_checks::enabled(request, revision).await?
            {
                checks.push(MergeCheck::Stack(revision.phid.as_str().to_owned()));
            }
        }
    }
    let phid = revision.phid.as_str().to_owned();
    // A new diff is checked instead of the stack.
    if let Some(update) = &actions.update {
        if merge_checks::enabled(request, revision).await? {
            checks.push(MergeCheck::Diff(phid, update.diff.phid.clone()));
        }
        return Ok(checks);
    }
    // Whether the revision lands at all changed.
    let abandons = matches!(actions.author_action, Some((AuthorAction::Abandon, _)))
        || actions.close.is_some();
    if (abandons || rewiring.count() > 0) && merge_checks::enabled(request, revision).await? {
        checks.push(MergeCheck::Stack(phid));
    }
    Ok(checks)
}

/// Every transaction of the edit, in the order PHP applies them: the
/// `core:create` of a new revision, the subscription (`array_unshift()`),
/// the actions, the links and files, the comment then the inline comments
/// (`sortTransactions()`), then the status (`applyFinalEffects()`). A
/// subscription of mentions alone comes after the actions instead. An
/// edit of nothing with an effect is left to PHP.
fn applied_order(
    (create, subscription, actions): (Option<&String>, Option<&Subscription>, &Actions),
    (links, file_edit): (&[Link], Option<&files::Planned>),
    (comment, inlines, status): (Option<&String>, Vec<String>, Option<&String>),
) -> Result<Vec<String>, MethodError> {
    let (first, late) = match subscription {
        Some(subscription) if subscription.first => (Some(subscription.phid.clone()), None),
        Some(subscription) => (None, Some(subscription.phid.clone())),
        None => (None, None),
    };
    let applied: Vec<String> = create
        .cloned()
        .into_iter()
        .chain(first)
        .chain(actions.phids())
        .chain(late)
        .chain(links.iter().map(|link| link.phid.clone()))
        .chain(file_edit.map(|edit| edit.phid.clone()))
        .chain(comment.cloned())
        .chain(inlines)
        .chain(status.cloned())
        .collect();
    if applied.is_empty() {
        return Err(MethodError::Unsupported(NO_EFFECT));
    }
    Ok(applied)
}

/// The revisions and files the edit's remarkup links to.
async fn plan_remarkup(
    request: &ApiRequest<'_>,
    finder: Option<FileFinder>,
    (wanted, revision): (&Wanted, &Revision),
    old_texts: &[(TextField, String)],
) -> Result<(Vec<Link>, Option<files::Planned>), MethodError> {
    let links = plan_links(request, wanted, revision).await?;
    let old = old_texts.first().map(|(_, old)| old.as_str());
    let new = (&new_texts(wanted)[..], old);
    let file_edit = files::plan(request, finder, revision.phid.as_str(), new).await?;
    Ok((links, file_edit))
}

/// The revision the edit applies to, new or existing, its facts, and the
/// old texts of the fields the edit sets. Unchanged texts are dropped
/// from `wanted`, when their remarkup would have no effect either (PHP
/// expands the remarkup of every text before it drops those without
/// effect); an existing revision gets one text with effect.
async fn load_target(
    request: &ApiRequest<'_>,
    wanted: &mut Wanted,
    is_new: bool,
    actor: &str,
) -> Result<(Revision, Facts, Vec<(TextField, String)>), MethodError> {
    let (revision, facts) = if is_new {
        check_new_texts(request, wanted)?;
        new_revision(request, actor)
    } else {
        let revision = load_revision(request).await?;
        let facts = Facts::load(&request.context.database, &revision).await?;
        (revision, facts)
    };
    check_side_effects(request, &revision, &facts).await?;
    let mut old_texts = Vec::new();
    let mut unchanged = Vec::new();
    for (field, new) in &wanted.texts {
        let planned = plan_text(request, &revision, *field, new).await;
        match drop_no_effect((request, wanted.continues), &revision, planned).await? {
            Some(old) => old_texts.push((*field, old)),
            None if is_inert(request, new) => unchanged.push(*field),
            None => {
                return Err(MethodError::Unsupported(
                    "unchanged texts with remarkup effects",
                ));
            }
        }
    }
    wanted.texts.retain(|(field, _)| !unchanged.contains(field));
    wanted
        .order
        .retain(|action| !matches!(action, Action::Text(field) if unchanged.contains(field)));
    if !is_new && wanted.texts.len() > 1 {
        return Err(MethodError::Unsupported("transactions not ported"));
    }
    Ok((revision, facts, old_texts))
}

/// The comment's text, transaction and comment PHIDs.
fn new_comment(text: String) -> (String, String, String) {
    let comment_phid = Phid::generate(PhidType::TRANSACTION_COMMENT, None).into_string();
    (text, transaction_phid(), comment_phid)
}

/// The planned review and author action.
type ReviewActions = (
    Option<(Review, String, Vec<ReviewerUpdate>)>,
    (Option<(AuthorAction, String)>, Option<PlannedVoid>),
);

/// The review and the author's action, with their transactions.
async fn plan_review_actions(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    (revision, facts): (&mut Revision, &mut Facts),
    actor: &str,
) -> Result<ReviewActions, MethodError> {
    let review = match wanted.review {
        Some(review) => {
            let updates = plan_review(request, review, revision, facts, actor).await?;
            Some((review, transaction_phid(), updates))
        }
        None => None,
    };
    let author = match wanted.author_action {
        Some(action) => plan_author(request, action, revision, facts, actor).await?,
        None => (None, None),
    };
    Ok((review, author))
}

/// The inline state, closing and buildable status transactions, and the
/// properties they set.
type StateEdits = (
    Option<(usize, inline_state::Planned)>,
    Option<String>,
    Option<BuildableStatus>,
    Vec<(String, Value)>,
);

/// The transactions which set states: the inline state the first comment,
/// inline comment or update expands (`expandTransaction()`), closing and
/// a buildable's status.
async fn plan_states(
    request: &ApiRequest<'_>,
    wanted: &mut Wanted,
    (revision, facts): (&mut Revision, &Facts),
    (actor, is_new): (&str, bool),
) -> Result<StateEdits, MethodError> {
    let inline_state = match wanted.state_trigger {
        Some(at) if !is_new => inline_state::plan(request, revision, actor)
            .await?
            .map(|state| (at, state)),
        _ => None,
    };
    let mut properties = Vec::new();
    let close = if wanted.close {
        properties.extend(plan_close(request, revision, facts, (actor, false)).await?);
        Some(transaction_phid())
    } else {
        None
    };
    let buildable = match wanted.buildable.take() {
        Some((buildable_phid, new)) => {
            let (old, edit) = plan_buildable(facts, &buildable_phid, new.as_deref())?;
            properties.push(edit);
            Some(BuildableStatus {
                phid: transaction_phid(),
                buildable_phid,
                old,
                new,
            })
        }
        None => None,
    };
    Ok((inline_state, close, buildable, properties))
}

/// The custom fields with an effect. A reviewer edit with a new diff of an
/// existing revision (`reviewed`) is left to PHP: the diff voids reviews,
/// which the edit would see.
async fn plan_custom_fields(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &Revision,
    (actor, reviewed, is_new): (&str, bool, bool),
) -> Result<Vec<custom_fields::Planned>, MethodError> {
    if reviewed && wanted.update.is_some() && !is_new {
        return Err(MethodError::Unsupported("several actions"));
    }
    let mut planned = Vec::new();
    for field in &wanted.custom_fields {
        match custom_fields::plan(request, revision, field, actor).await? {
            Some(edit) => planned.push(edit),
            None if !wanted.continues => return Err(MethodError::Unsupported(NO_EFFECT)),
            None => {}
        }
    }
    Ok(planned)
}

/// How an edit is made.
struct Editor {
    /// Where it comes from.
    source: Source,
    /// Whether it publishes the actor's draft inline comments (the comment
    /// form).
    drafts: bool,
    /// What the omnipotent actor acts as (`setActingAsPHID()`).
    acting_as: Option<String>,
}

impl Editor {
    /// An edit of the `transactions` of a call.
    fn of_request(source: Source, drafts: bool) -> Self {
        Self {
            source,
            drafts,
            acting_as: None,
        }
    }
}

async fn plan(
    request: &ApiRequest<'_>,
    finder: Option<FileFinder>,
    editor: Editor,
) -> Result<Plan, MethodError> {
    let is_new = is_creation(request)?;
    let wanted = wanted(request, (is_new, editor.drafts), &editor.source)?;
    plan_wanted(request, finder, editor, (wanted, is_new)).await
}

/// Plans the `wanted` transactions.
async fn plan_wanted(
    request: &ApiRequest<'_>,
    finder: Option<FileFinder>,
    editor: Editor,
    (mut wanted, is_new): (Wanted, bool),
) -> Result<Plan, MethodError> {
    let (source, drafts) = (editor.source, editor.drafts);
    let (actor, edit_policy) = actor_phid(request, editor.acting_as)?;
    let database = &request.context.database;
    // The web forms' editor raises an error for what has no effect.
    wanted.continues = !matches!(source, Source::Web { .. });
    let (mut revision, mut facts, old_texts) =
        load_target(request, &mut wanted, is_new, &actor).await?;
    let inlines = plan_inlines(request, &mut wanted, &revision, drafts, &actor).await?;
    let mut subscription = plan_subscription(
        request,
        &revision,
        &mut facts,
        (&wanted, &old_texts),
        &actor,
    )
    .await?;
    let (review, (author_action, void)) =
        plan_review_actions(request, &wanted, (&mut revision, &mut facts), &actor).await?;
    let (inline_state, close, buildable, property_edits) = plan_states(
        request,
        &mut wanted,
        (&mut revision, &facts),
        (&actor, is_new),
    )
    .await?;
    let (title, texts) = plan_fields(request, &wanted, &mut revision, &old_texts).await?;
    let (reviewer_edit, subscriber_edit, tag_edit) = plan_lists(
        request,
        &wanted,
        &mut subscription,
        (&mut revision, &mut facts),
        &actor,
    )
    .await?;
    let reviewed = reviewer_edit.is_some();
    let custom =
        plan_custom_fields(request, &wanted, &revision, (&actor, reviewed, is_new)).await?;
    let (links, file_edit) =
        plan_remarkup(request, finder, (&wanted, &revision), &old_texts).await?;
    // The update's internal effects come after every check of the edit.
    let update = plan_update(request, &wanted, &mut revision, &facts, &actor).await?;
    let properties = final_properties(&facts, update.as_ref(), property_edits)?;
    let comment = wanted.comment.map(new_comment);
    let status = plan_status(request, &mut revision);
    let actions = Actions {
        order: wanted.order,
        review,
        author_action,
        void,
        title,
        texts,
        update,
        reviewer_edit,
        subscriber_edit,
        tag_edit,
        close,
        buildable,
        inline_state,
        custom_fields: custom,
    };
    // The engine's `core:create` comes first.
    let create = is_new.then(transaction_phid);
    let applied_phids = applied_order(
        (create.as_ref(), subscription.as_ref(), &actions),
        (&links, file_edit.as_ref()),
        (
            comment.as_ref().map(|(_, phid, _)| phid),
            inlines.phids(),
            status.as_ref().map(|(phid, _)| phid),
        ),
    )?;
    let merge_checks = plan_merge_checks(request, &revision, &actions, &links).await?;
    let input = herald_input((&actor, is_new, &source), &actions, subscription.as_ref());
    let published = publication(request, &mut revision, &mut facts, &input, &applied_phids).await?;
    let via = (actor.as_str(), source.constant());
    let tasks = link_tasks(database, links, via, &published.must_encrypt).await?;
    Ok(Plan {
        source,
        actor,
        edit_policy,
        properties,
        create,
        group_id: random::random_characters(32),
        subscription,
        actions,
        links: tasks,
        file_edit,
        merge_checks,
        comment,
        inlines,
        status,
        applied_phids,
        rules: published.rules,
        transcript: published.transcript,
        herald_header: published.herald_header,
        worker_data: published.worker_data,
        herald: published.herald,
        herald_remembered: published.herald_remembered,
        revision,
    })
}

/// The inline comments the edit publishes, on the revision before the
/// edit (an update doesn't move them): those of its `inline` transactions,
/// or the actor's drafts. Their texts join the edit's.
async fn plan_inlines(
    request: &ApiRequest<'_>,
    wanted: &mut Wanted,
    revision: &Revision,
    drafts: bool,
    actor: &str,
) -> Result<inlines::Inlines, MethodError> {
    let planned = if drafts {
        let planned = inlines::drafts(request, revision, actor).await?;
        // newAutomaticCommentTransactions() come before the comment.
        wanted.comment_position = planned.planned.len();
        planned
    } else {
        inlines::plan(request, revision, &wanted.inlines, actor).await?
    };
    if wanted.empty_comment && planned.planned.is_empty() {
        return Err(MethodError::Unsupported("empty comments"));
    }
    // getLegacyRequiredCapabilities(): CAN_EDIT, or a policy exception.
    if !planned.planned.is_empty() && !can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    wanted.inline_texts = planned.texts();
    Ok(planned)
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
    .bind(&plan.edit_policy)
    .bind(kind)
    .bind(old)
    .bind(new)
    .bind(plan.source.serialized())
    .bind(metadata)
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The metadata of a transaction of the edit's group: `entries`, then
/// `core.create` for a new revision (`setIsCreateTransaction()`).
fn group_metadata(plan: &Plan, entries: Value) -> String {
    let mut metadata = match entries {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if plan.create.is_some() {
        metadata.insert("core.create".to_owned(), json!(true));
    }
    metadata.insert("core.groupID".to_owned(), json!(plan.group_id));
    php::json_encode(&Value::Object(metadata))
}

/// `$object->save()` of a new revision (`initializeNewRevision()` and the
/// edit's internal effects). Returns its ID.
async fn insert_revision(
    request: &ApiRequest<'_>,
    plan: &Plan,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<i64, MethodError> {
    let database = &request.context.database;
    let revision = &plan.revision;
    let update = plan
        .actions
        .update
        .as_ref()
        .ok_or_else(|| MethodError::internal("a new revision without a diff"))?;
    let text = |field| {
        plan.actions
            .texts
            .iter()
            .find(|(known, ..)| *known == field)
            .map(|(_, _, _, new)| new.as_str())
            .unwrap_or_default()
    };
    let id = phab_db::sqlx::query(&format!(
        "INSERT INTO {} (title, phid, status, summary, testPlan, authorPHID, \
         lastReviewerPHID, lineCount, dateCreated, dateModified, attached, mailKey, branchName, \
         viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES (?, ?, ?, \
         ?, ?, ?, NULL, ?, ?, ?, '[]', ?, NULL, ?, ?, ?, ?, ?)",
        database.table("differential", "differential_revision")
    ))
    .bind(&revision.title)
    .bind(revision.phid.as_str())
    .bind(&revision.status)
    .bind(text(TextField::Summary))
    .bind(text(TextField::TestPlan))
    .bind(&plan.actor)
    .bind(update.diff.line_count)
    .bind(request.now)
    .bind(request.now)
    .bind(random::random_characters(40))
    .bind(&revision.view_policy)
    .bind(&revision.edit_policy)
    .bind(&update.diff.repository_phid)
    .bind(&plan.properties)
    .bind(&update.diff.phid)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    i64::try_from(id).map_err(MethodError::internal)
}

/// Writes the planned edit in one transaction. A new revision gets its ID.
async fn write(request: &ApiRequest<'_>, plan: &mut Plan) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    if plan.create.is_some() {
        let id = insert_revision(request, plan, connection).await?;
        plan.revision.id = id;
        set_monogram(&mut plan.worker_data, id);
    } else {
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET status = ?, title = ?, properties = COALESCE(?, properties), \
             dateModified = ? WHERE id = ?",
            database.table("differential", "differential_revision")
        ))
        .bind(&plan.revision.status)
        .bind(&plan.revision.title)
        .bind(&plan.properties)
        .bind(now)
        .bind(plan.revision.id)
        .execute(&mut *connection)
        .await?;
        if let Some(update) = &plan.actions.update {
            update::write_revision(request, plan, update, connection).await?;
        }
    }
    let plan = &*plan;
    let revision = &plan.revision;
    let metadata = group_metadata(plan, json!({}));
    if let Some(phid) = &plan.create {
        let row = (phid.as_str(), "core:create", "null", "null", &*metadata);
        insert_transaction(request, plan, connection, row).await?;
    }
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
    write_comments(request, plan, &metadata, connection).await?;
    for (link, _) in &plan.links {
        write_link(request, plan, link, connection).await?;
    }
    if let Some(update) = &plan.actions.update {
        update::write_final(request, plan, update, connection).await?;
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
    // didCommitTransaction().
    if let Some(update) = &plan.actions.update {
        update::write_view_states(request, plan, update, connection).await?;
    }
    for check in &plan.merge_checks {
        match check {
            MergeCheck::Stack(phid) => {
                merge_checks::queue(request, connection, std::slice::from_ref(phid)).await?;
            }
            MergeCheck::Diff(phid, diff) => {
                merge_checks::queue_diff(request, connection, phid, diff).await?;
            }
        }
    }
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
    let state = plan.actions.inline_state.as_ref();
    for (index, action) in plan.actions.order.iter().enumerate() {
        if let Some((_, state)) = state.filter(|(at, _)| *at == index) {
            inline_state::write(request, plan, state, connection).await?;
        }
        write_action(request, plan, *action, metadata, connection).await?;
    }
    // "Request Review", the only action, is followed by its void.
    if let Some((phid, reviewers, force)) = &plan.actions.void {
        write_void(request, plan, (phid, reviewers, force), connection).await?;
    }
    if let Some((_, state)) = state.filter(|(at, _)| *at >= plan.actions.order.len()) {
        inline_state::write(request, plan, state, connection).await?;
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
        Action::Update => {
            if let Some(update) = &actions.update {
                // The edit form's diff is the value its `update` field
                // starts with: a default transaction
                // (`setIsDefaultTransaction()`).
                let default;
                let metadata = if plan.source == Source::EditForm {
                    default = group_metadata(plan, json!({"core.default": true}));
                    &default
                } else {
                    metadata
                };
                update::write_transaction(request, plan, update, metadata, connection).await?;
            }
        }
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
        Action::Custom(_) | Action::Buildable | Action::Close => {
            write_metadata_action(request, plan, action, connection).await?;
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
        Action::Text(field) => {
            // The edit form's remarkup control posts `{}`, which decodes to
            // an empty list.
            let form_metadata = matches!(plan.source, Source::EditForm)
                .then(|| group_metadata(plan, json!({"remarkup.control": []})));
            let metadata = form_metadata.as_deref().unwrap_or(metadata);
            for text in actions.texts.iter().filter(|(known, ..)| *known == field) {
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

/// The transaction of an action with its own metadata: closing, a
/// buildable's status, a custom field.
async fn write_metadata_action(
    request: &ApiRequest<'_>,
    plan: &Plan,
    action: Action,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let actions = &plan.actions;
    match action {
        Action::Custom(field) => {
            let edits = actions.custom_fields.iter();
            for edit in edits.filter(|edit| edit.field() == field) {
                custom_fields::write(request, plan, edit, connection).await?;
            }
        }
        Action::Buildable => {
            if let Some(edit) = &actions.buildable {
                let entries = json!({"harbormaster:buildablePHID": edit.buildable_phid});
                let metadata = group_metadata(plan, entries);
                let (old, new) = (
                    php::json_encode(&json!(edit.old)),
                    php::json_encode(&json!(edit.new)),
                );
                let row = (
                    edit.phid.as_str(),
                    "harbormaster:buildable",
                    &*old,
                    &*new,
                    &*metadata,
                );
                insert_transaction(request, plan, connection, row).await?;
            }
        }
        Action::Close => {
            if let Some(close) = &actions.close {
                let metadata = group_metadata(plan, json!({}));
                let kind = "differential.revision.close";
                let row = (close.as_str(), kind, "false", "true", &*metadata);
                insert_transaction(request, plan, connection, row).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The metadata of an edge transaction of the edit's group.
fn edge_metadata(plan: &Plan, edge_type: i64) -> String {
    group_metadata(plan, json!({"edge:type": edge_type}))
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
    // The sub-editors' transactions edit existing objects.
    let inverse = php::json_encode(&json!({
        "edge:type": inverse_edge_type(link.edge_type)?,
        "core.groupID": plan.group_id,
    }));
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
    let metadata = group_metadata(plan, json!({"void.force": force}));
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
    let data = php::json_encode(&plan.worker_data);
    worker::schedule_task(connection, database, publish(&data, revision), request.now).await?;
    for (link, data) in &plan.links {
        for (target, data) in link.targets.iter().zip(data) {
            let target = target.phid();
            worker::schedule_task(connection, database, publish(data, target), request.now).await?;
        }
    }
    Ok(())
}

/// The comment, then the inline comments (with the group's `metadata`),
/// and `markReviewerComments()`.
async fn write_comments(
    request: &ApiRequest<'_>,
    plan: &Plan,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    if let Some((text, phid, comment_phid)) = &plan.comment {
        let metadata = group_metadata(plan, plan.source.comment_metadata());
        write_comment(
            request,
            plan,
            (text, phid, comment_phid),
            &metadata,
            connection,
        )
        .await?;
    }
    if !plan.inlines.planned.is_empty() {
        inlines::write(request, plan, metadata, connection).await?;
    }
    if plan.comment.is_some() || !plan.inlines.planned.is_empty() {
        mark_reviewer_comments(request, plan, connection).await?;
    }
    Ok(())
}

/// The comment and its transaction.
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
    .bind(&plan.edit_policy)
    .bind(comment_phid)
    .bind(plan.source.serialized())
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
    .bind(plan.source.serialized())
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// `markReviewerComments()`: the actor commented on the active diff.
async fn mark_reviewer_comments(
    request: &ApiRequest<'_>,
    plan: &Plan,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = &plan.revision;
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
    // applyEffects(): rules repeating on every edit or on change forget
    // where they applied; those repeating the first time or on change
    // remember where they applied now.
    let forget: Vec<&str> = plan
        .rules
        .iter()
        .filter(|rule| rule.repetition_policy != "first")
        .map(|rule| rule.id.as_str())
        .collect();
    if !forget.is_empty() {
        phab_db::bind_all(
            phab_db::sqlx::query(&format!(
                "DELETE FROM {} WHERE phid = ? AND ruleID IN ({})",
                database.table("herald", "herald_ruleapplied"),
                phab_db::placeholders(forget.len())
            ))
            .bind(revision.phid.as_str()),
            forget.iter().copied(),
        )
        .execute(&mut *connection)
        .await?;
    }
    for rule_id in &plan.herald_remembered {
        phab_db::sqlx::query(&format!(
            "INSERT IGNORE INTO {} (phid, ruleID) VALUES (?, ?)",
            database.table("herald", "herald_ruleapplied")
        ))
        .bind(revision.phid.as_str())
        .bind(rule_id)
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

/// Why a buildable's status isn't published: the revision has it already.
const BUILDABLE_UNCHANGED: &str = "buildable statuses the revision has";

/// `getBuildableStatus()` of the buildable `phid` in the revision's
/// `buildables` property, and the property once `setBuildableStatus()`
/// sets it to `new`.
fn plan_buildable(
    facts: &Facts,
    phid: &str,
    new: Option<&str>,
) -> Result<(Option<String>, (String, Value)), MethodError> {
    let unusual = || MethodError::Unsupported("unusual revision properties");
    let properties = facts.properties.as_ref().ok_or_else(unusual)?;
    // A value which isn't an array starts over; an empty one is PHP's
    // empty array.
    let as_map = |value: Option<&Value>| match value {
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(Value::Array(list)) if !list.is_empty() => Err(unusual()),
        _ => Ok(Map::new()),
    };
    let mut buildables = as_map(properties.get("buildables"))?;
    let mut buildable = as_map(buildables.get(phid))?;
    let old = match buildable.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(status)) => Some(status.clone()),
        Some(_) => return Err(unusual()),
    };
    if old.as_deref() == new {
        return Err(MethodError::Unsupported(BUILDABLE_UNCHANGED));
    }
    buildable.insert("status".to_owned(), json!(new));
    buildables.insert(phid.to_owned(), Value::Object(buildable));
    Ok((old, ("buildables".to_owned(), Value::Object(buildables))))
}

/// `PhabricatorHarbormasterApplication`'s PHID, which Harbormaster's
/// daemons act as.
const HARBORMASTER_APP: &str = "PHID-APPS-PhabricatorHarbormasterApplication";

/// A request of the daemons' omnipotent viewer on revision `id`.
fn daemon_request(context: &Context, id: i64, now: i64) -> ApiRequest<'_> {
    let mut params = Map::new();
    params.insert("objectIdentifier".to_owned(), json!(id));
    ApiRequest {
        context,
        user: None,
        viewer: phab_policy::Viewer::omnipotent(),
        params,
        strictly_typed: true,
        now,
        writes: phab_auth::Writes::Allowed,
    }
}

/// Plans an edit of the daemons acting as `acting_as` on revision `id`,
/// and writes it when `write_it`.
async fn daemon_edit(
    context: &Context,
    (id, acting_as): (i64, &str),
    wanted: Wanted,
    (now, write_it): (i64, bool),
) -> Result<(), MethodError> {
    let request = daemon_request(context, id, now);
    let editor = Editor {
        source: Source::Daemon,
        drafts: false,
        acting_as: Some(acting_as.to_owned()),
    };
    let mut plan = plan_wanted(&request, None, editor, (wanted, false)).await?;
    if write_it {
        write(&request, &mut plan).await?;
    }
    Ok(())
}

/// `DifferentialBuildableEngine::publishBuildable()`, once Harbormaster
/// decided to publish the buildable `buildable_phid` (automatic, done
/// building) of a diff of revision `revision_id`: when the revision's
/// status of the buildable isn't `status` already, the
/// `harbormaster:buildable` transaction setting it, applied as the
/// Harbormaster application by the omnipotent viewer, with the daemons'
/// content source, at `now`. Only plans it unless `write_it`. What isn't
/// ported is [`MethodError::Unsupported`], before anything is written.
pub async fn publish_buildable(
    context: &Context,
    revision_id: i64,
    (buildable_phid, status): (&str, Option<&str>),
    (now, write_it): (i64, bool),
) -> Result<(), MethodError> {
    let wanted = Wanted {
        order: vec![Action::Buildable],
        buildable: Some((buildable_phid.to_owned(), status.map(str::to_owned))),
        ..Wanted::default()
    };
    match daemon_edit(
        context,
        (revision_id, HARBORMASTER_APP),
        wanted,
        (now, write_it),
    )
    .await
    {
        Err(MethodError::Unsupported(BUILDABLE_UNCHANGED)) => Ok(()),
        result => result,
    }
}

pub use files::{FileFinder, FilesFuture};

/// Plans and writes the edit the `objectIdentifier` and `transactions` of
/// `request` ask for, as made from `source`: `differential.revision.edit`
/// for [`Source::Conduit`], the forms of the web interface (the comment
/// form, the subscribe action) for [`Source::Web`], once they made their
/// transactions Conduit ones. Without `files`, references to files are
/// left to PHP. [`MethodError::Unsupported`] comes before anything is
/// written. Returns the method's result.
pub async fn apply(
    request: &ApiRequest<'_>,
    files: Option<FileFinder>,
    source: Source,
) -> Result<Value, MethodError> {
    let mut plan = plan(request, files, Editor::of_request(source, false)).await?;
    write(request, &mut plan).await?;
    Ok(result(&plan))
}

/// [`apply()`] for the comment form, which also publishes the actor's
/// draft inline comments (`newAutomaticCommentTransactions()`): an empty
/// comment is only kept when there is none.
pub async fn apply_with_drafts(
    request: &ApiRequest<'_>,
    files: Option<FileFinder>,
    source: Source,
) -> Result<Value, MethodError> {
    let mut plan = plan(request, files, Editor::of_request(source, true)).await?;
    write(request, &mut plan).await?;
    Ok(result(&plan))
}

/// The method's result: the revision and every transaction.
fn result(plan: &Plan) -> Value {
    json!({
        "object": {"id": plan.revision.id, "phid": plan.revision.phid.as_str()},
        "transactions": plan
            .applied_phids
            .iter()
            .map(|phid| json!({"phid": phid}))
            .collect::<Vec<_>>(),
    })
}

/// Whether `text` reads as fields of a commit message, which PHP refuses
/// in a summary or a test plan (`validateMessageCorpus()`), even unchanged.
pub fn names_commit_message_fields(
    config: &phab_core::Config,
    text: &str,
) -> Result<bool, MethodError> {
    crate::commit_message::parse::names_fields(config, text)
}

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
        Box::pin(async move { apply(request, self.files, Source::Conduit).await })
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
