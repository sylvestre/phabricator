//! The actions of Herald rules which edit the revision
//! (`PhabricatorSubscriptionsHeraldAction`,
//! `DifferentialReviewersHeraldAction`, `PhabricatorProjectHeraldAction`),
//! and those which mail (`PhabricatorMetaMTAEmailHeraldAction`): each logs
//! what it did
//! (`logEffect()`, kept in the transcript) and queues a transaction, which
//! PHP's Herald editor applies after the edit's, as the Herald
//! application (`applyHeraldRules()`).

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, php};
use phab_db::phpser::{Key, Value as Php};
use phab_db::{edges, rows};
use serde_json::json;

use phab_app_herald::engine::Effect;

use super::{Facts, Plan, Subscription};
use crate::storage::Revision;

/// `PhabricatorHeraldApplication`'s PHID, the author of its transactions.
const HERALD_PHID: &str = "PHID-APPS-PhabricatorHeraldApplication";

/// `PhabricatorHeraldContentSource`.
const HERALD_SOURCE: &str = r#"{"source":"herald","params":[]}"#;

/// A transaction an action queues.
enum Entry {
    /// Subscriptions: `combineTransactions()` merges them into the first.
    Subscribers,
    /// Reviewers, with the status each gets, `added` or `blocking`: these
    /// are not merged.
    Reviewers(Vec<(String, String)>),
    /// Tags: edge transactions of one type merge into the first.
    Projects,
}

/// What the actions queued, in order, and the merged subscriptions' `+`
/// and `-` (a later transaction's PHIDs first).
#[derive(Default)]
pub(super) struct Queue {
    entries: Vec<Entry>,
    add: Vec<String>,
    remove: Vec<String>,
    tag: Vec<String>,
    untag: Vec<String>,
    /// `addEmailPHID()`: who gets mail, and who gets it however they
    /// configured their mail.
    email: Vec<String>,
    forced_email: Vec<String>,
}

impl Queue {
    /// Queues a mergeable transaction, the first of its kind.
    fn merged(&mut self, entry: Entry, kind: fn(&Entry) -> bool) {
        if !self.entries.iter().any(kind) {
            self.entries.push(entry);
        }
    }
}

/// `$merged = array_merge($later, $earlier)` of PHIDs.
fn merge(into: &mut Vec<String>, later: &[String]) {
    let mut merged = later.to_vec();
    merged.extend(into.iter().filter(|phid| !later.contains(phid)).cloned());
    *into = merged;
}

/// A transaction of the Herald editor.
enum Planned {
    Subscription(Subscription),
    Reviewers(super::reviewers::Planned),
    Tags(super::tags::Planned),
}

/// The Herald editor's transactions, in the order it applies them, and
/// what they leave of the reviewers.
#[derive(Default)]
pub(super) struct Edits {
    transactions: Vec<Planned>,
    /// The reviewers before and after the transactions, to write.
    reviewers: Option<super::reviewers::Planned>,
    /// The publish task's `heraldEmailPHIDs` and `heraldForcedEmailPHIDs`.
    pub(super) email: Vec<String>,
    pub(super) forced_email: Vec<String>,
}

impl Edits {
    /// Their PHIDs, in order.
    pub(super) fn phids(&self) -> Vec<String> {
        self.transactions
            .iter()
            .map(|planned| match planned {
                Planned::Subscription(edit) => edit.phid.clone(),
                Planned::Reviewers(edit) => edit.phid.clone(),
                Planned::Tags(edit) => edit.phid.clone(),
            })
            .collect()
    }
}

fn keyed(phids: &[String]) -> Php {
    Php::Array(
        phids
            .iter()
            .map(|phid| (Key::from(phid.as_str()), Php::str(phid)))
            .collect(),
    )
}

fn list(phids: &[String]) -> Php {
    Php::list(phids.iter().map(|phid| Php::str(phid)))
}

/// Applies one effect of a rule which matched: logs it and queues what it
/// does. `removes_subscribers` says whether the edit unsubscribes anyone:
/// the unsubscribers it makes come in edge order, so adding subscribers
/// then is left to PHP.
pub(super) async fn apply(
    request: &ApiRequest<'_>,
    subject: (&Revision, &Facts),
    effect: &Effect,
    removes_subscribers: bool,
    (queue, must_encrypt): (&mut Queue, &mut Vec<String>),
) -> Result<Vec<(String, Php)>, MethodError> {
    let target = &effect.target;
    match effect.action.as_str() {
        "email.must-encrypt" => {
            must_encrypt.push(effect.rule_phid.clone());
            Ok(vec![(
                "do.must-encrypt".to_owned(),
                Php::list([Php::str(&effect.rule_phid)]),
            )])
        }
        "nothing" => Ok(vec![("do.nothing".to_owned(), Php::Null)]),
        "email.other" => email(request, subject.0, target, queue).await,
        action @ ("projects.add" | "projects.remove") => {
            tag(request, subject, (target, action == "projects.add"), queue).await
        }
        action @ ("differential.reviewers.add" | "differential.reviewers.blocking") => {
            let blocking = action == "differential.reviewers.blocking";
            review(request, subject, (target, blocking), queue).await
        }
        action @ ("subscribers.add" | "subscribers.remove") => {
            let is_add = action == "subscribers.add";
            if is_add && removes_subscribers {
                return Err(MethodError::Unsupported(
                    "Herald subscriptions of edits which unsubscribe",
                ));
            }
            subscribe(request, subject, (target, is_add), queue).await
        }
        _ => Err(MethodError::Unsupported(
            "Herald effects which edit the revision",
        )),
    }
}

/// `loadStandardTargets()` for users and projects: the targets which can
/// be acted on, and the log of those which can't.
async fn standard_targets(
    request: &ApiRequest<'_>,
    revision: &Revision,
    (phids, projects_only): (Vec<String>, bool),
    log: &mut Vec<(String, Php)>,
) -> Result<Vec<String>, MethodError> {
    if phids.is_empty() {
        log.push(("do.standard.empty".to_owned(), Php::Null));
        return Ok(Vec::new());
    }
    let (valid, invalid): (Vec<String>, Vec<String>) = phids.into_iter().partition(|phid| {
        phid.starts_with("PHID-PROJ-") || (!projects_only && phid.starts_with("PHID-USER-"))
    });
    if !invalid.is_empty() {
        log.push(("do.standard.invalid".to_owned(), list(&invalid)));
    }
    if valid.is_empty() {
        return Ok(Vec::new());
    }
    let database = &request.context.database;
    let mut loaded = Vec::new();
    let mut unloadable = Vec::new();
    for phid in valid {
        let (database_name, table) = if phid.starts_with("PHID-USER-") {
            ("user", "user")
        } else {
            ("project", "project")
        };
        let sql = format!(
            "SELECT isAdmin AS admin FROM {} WHERE phid = ?",
            database.table(database_name, table)
        );
        let sql = if table == "project" {
            sql.replace("isAdmin AS admin", "0 AS admin")
        } else {
            sql
        };
        match phab_db::sqlx::query(&sql)
            .bind(&phid)
            .fetch_optional(database.pool())
            .await?
        {
            Some(row) => loaded.push((phid, rows::flag(&row, "admin")?)),
            None => unloadable.push(phid),
        }
    }
    if !unloadable.is_empty() {
        log.push(("do.standard.unloadable".to_owned(), list(&unloadable)));
    }
    if loaded.is_empty() {
        return Ok(Vec::new());
    }
    // Users who can't see the revision are no targets.
    let context = request.context;
    let mut targets = Vec::new();
    let mut no_permission = Vec::new();
    for (phid, admin) in loaded {
        if phid.starts_with("PHID-USER-") {
            let viewer = phab_policy::Viewer::user(Phid::new(phid.clone()), admin);
            let visible = crate::storage::visible_revisions(
                database,
                &context.config,
                &viewer,
                vec![revision.clone()],
                request.now,
            )
            .await
            .map_err(MethodError::internal)?;
            if visible.is_empty() {
                no_permission.push(phid);
                continue;
            }
        }
        targets.push(phid);
    }
    if !no_permission.is_empty() {
        log.push(("do.standard.permission".to_owned(), list(&no_permission)));
    }
    Ok(targets)
}

/// `applySubscribe()`: `subscribers.add` or `subscribers.remove` of
/// `targets`, against the revision as the edit leaves it. Returns the log.
pub(super) async fn subscribe(
    request: &ApiRequest<'_>,
    (revision, facts): (&Revision, &Facts),
    (targets, is_add): (&Php, bool),
    queue: &mut Queue,
) -> Result<Vec<(String, Php)>, MethodError> {
    let mut log = Vec::new();
    let phids = target_phids(targets);
    // PhabricatorObjectQuery orders several targets by type and query.
    if phids.len() > 1 {
        return Err(MethodError::Unsupported(
            "Herald actions on several targets",
        ));
    }
    let mut phids = standard_targets(request, revision, (phids, false), &mut log).await?;
    if phids.is_empty() {
        return Ok(log);
    }
    if is_add {
        // PHP logs every unsubscriber of the object, as it fills the list
        // of all of them with the targets found in it.
        let unsubscribed = unsubscribers(request, revision, facts).await?;
        phids.retain(|phid| !unsubscribed.contains(phid));
        if !unsubscribed.is_empty() {
            log.push(("do.previously-unsubscribed".to_owned(), list(&unsubscribed)));
        }
        if phids.is_empty() {
            return Ok(log);
        }
    }
    let automatic: Vec<String> = phids
        .iter()
        .filter(|phid| {
            revision
                .author_phid
                .as_ref()
                .is_some_and(|author| author.as_str() == phid.as_str())
                || facts
                    .reviewers
                    .iter()
                    .any(|reviewer| reviewer.reviewer == **phid && reviewer.status != "resigned")
        })
        .cloned()
        .collect();
    phids.retain(|phid| !automatic.contains(phid));
    if !automatic.is_empty() {
        log.push(("do.autosubscribed".to_owned(), list(&automatic)));
    }
    if phids.is_empty() {
        return Ok(log);
    }
    let (already, remaining): (Vec<String>, Vec<String>) = phids
        .into_iter()
        .partition(|phid| facts.subscribers.contains(phid) == is_add);
    if !already.is_empty() {
        log.push(("do.standard.no-effect".to_owned(), keyed(&already)));
    }
    if remaining.is_empty() {
        return Ok(log);
    }
    queue.merged(Entry::Subscribers, |entry| {
        matches!(entry, Entry::Subscribers)
    });
    merge(
        if is_add {
            &mut queue.add
        } else {
            &mut queue.remove
        },
        &remaining,
    );
    let code = if is_add {
        "do.subscribed"
    } else {
        "do.unsubscribed"
    };
    log.push((code.to_owned(), keyed(&remaining)));
    Ok(log)
}

/// `applyReviewers()`: `differential.reviewers.add` or `.blocking` of
/// `targets`. Returns the log.
pub(super) async fn review(
    request: &ApiRequest<'_>,
    (revision, facts): (&Revision, &Facts),
    (targets, is_blocking): (&Php, bool),
    queue: &mut Queue,
) -> Result<Vec<(String, Php)>, MethodError> {
    let mut log = Vec::new();
    let mut phids = target_phids(targets);
    if phids.len() > 1 {
        return Err(MethodError::Unsupported(
            "Herald actions on several targets",
        ));
    }
    // Authors don't review their revisions.
    let author = revision.author_phid.as_ref().map(Phid::as_str);
    let authors: Vec<String> = phids
        .iter()
        .filter(|phid| Some(phid.as_str()) == author)
        .cloned()
        .collect();
    if !authors.is_empty() {
        phids.retain(|phid| Some(phid.as_str()) != author);
        log.push(("do.authors".to_owned(), list(&authors)));
        if phids.is_empty() {
            return Ok(log);
        }
    }
    let status = if is_blocking { "blocking" } else { "added" };
    // Reviewers whose status is stronger are no effect; an equal one is
    // queued again.
    let current: Vec<String> = phids
        .iter()
        .filter(|phid| {
            facts
                .reviewers
                .iter()
                .find(|reviewer| reviewer.reviewer == **phid)
                .is_some_and(|reviewer| strength(&reviewer.status) > strength(status))
        })
        .cloned()
        .collect();
    let mut no_effect = Vec::new();
    phids.retain(|phid| {
        let skip = current.contains(phid);
        if skip {
            no_effect.push(phid.clone());
        }
        !skip
    });
    if !no_effect.is_empty() {
        log.push(("do.standard.no-effect".to_owned(), list(&no_effect)));
        if phids.is_empty() {
            return Ok(log);
        }
    }
    let targets = standard_targets(request, revision, (phids, false), &mut log).await?;
    if targets.is_empty() {
        return Ok(log);
    }
    queue.entries.push(Entry::Reviewers(
        targets
            .iter()
            .map(|phid| (phid.clone(), status.to_owned()))
            .collect(),
    ));
    let code = if is_blocking {
        "do.add-blocking-reviewers"
    } else {
        "do.add-reviewers"
    };
    log.push((code.to_owned(), keyed(&targets)));
    Ok(log)
}

/// `applyProjects()`: `projects.add` or `projects.remove` of `targets`.
/// Returns the log.
pub(super) async fn tag(
    request: &ApiRequest<'_>,
    (revision, facts): (&Revision, &Facts),
    (targets, is_add): (&Php, bool),
    queue: &mut Queue,
) -> Result<Vec<(String, Php)>, MethodError> {
    let mut log = Vec::new();
    let phids = target_phids(targets);
    if phids.len() > 1 {
        return Err(MethodError::Unsupported(
            "Herald actions on several targets",
        ));
    }
    let phids = standard_targets(request, revision, (phids, true), &mut log).await?;
    if phids.is_empty() {
        return Ok(log);
    }
    let (already, remaining): (Vec<String>, Vec<String>) = phids
        .into_iter()
        .partition(|phid| facts.projects.contains(phid) == is_add);
    if !already.is_empty() {
        log.push(("do.standard.no-effect".to_owned(), keyed(&already)));
    }
    if remaining.is_empty() {
        return Ok(log);
    }
    if is_add {
        super::tags::check_added(request, &remaining).await?;
    }
    queue.merged(Entry::Projects, |entry| matches!(entry, Entry::Projects));
    merge(
        if is_add {
            &mut queue.tag
        } else {
            &mut queue.untag
        },
        &remaining,
    );
    let code = if is_add {
        "do.add-projects"
    } else {
        "do.remove-projects"
    };
    log.push((code.to_owned(), keyed(&remaining)));
    Ok(log)
}

/// `applyEmail()`: `email.other`. Returns the log.
pub(super) async fn email(
    request: &ApiRequest<'_>,
    revision: &Revision,
    targets: &Php,
    queue: &mut Queue,
) -> Result<Vec<(String, Php)>, MethodError> {
    let mut log = Vec::new();
    let phids = target_phids(targets);
    if phids.len() > 1 {
        return Err(MethodError::Unsupported(
            "Herald actions on several targets",
        ));
    }
    let phids = standard_targets(request, revision, (phids, false), &mut log).await?;
    if phids.is_empty() {
        return Ok(log);
    }
    for phid in &phids {
        if !queue.email.contains(phid) {
            queue.email.push(phid.clone());
        }
    }
    log.push(("do.send".to_owned(), keyed(&phids)));
    Ok(log)
}

/// `DifferentialReviewerStatus::getStatusStrength()`.
fn strength(status: &str) -> i64 {
    match status {
        "added" => 1,
        "commented" => 2,
        "blocking" => 3,
        "accepted-older" | "rejected-older" => 4,
        "accepted" | "rejected" | "resigned" => 5,
        _ => 0,
    }
}

/// The PHIDs of an action's target.
fn target_phids(targets: &Php) -> Vec<String> {
    match targets {
        Php::Array(entries) => entries
            .iter()
            .filter_map(|(_, value)| value.as_bytes())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// The unsubscribers of the revision after the edit: subscribing removes
/// the edge, unsubscribing adds it.
async fn unsubscribers(
    request: &ApiRequest<'_>,
    revision: &Revision,
    facts: &Facts,
) -> Result<Vec<String>, MethodError> {
    let database = &request.context.database;
    let mut found =
        super::destinations(database, &revision.phid, edges::OBJECT_HAS_UNSUBSCRIBER).await?;
    found.retain(|phid| !facts.subscribers.contains(phid));
    Ok(found)
}

/// The Herald editor's transactions of the queue, those with an effect,
/// applied to the revision and its facts as PHP's Herald editor applies
/// them. A review status they would change is left to PHP.
pub(super) fn edits(
    queue: Queue,
    (revision, facts): (&mut Revision, &mut Facts),
    (subscribed, tagged): (&[String], &[String]),
    sticky_accept: bool,
) -> Result<Edits, MethodError> {
    // populateTransaction() reads every old value before applying any.
    let before = super::reviewers::current(&facts.reviewers);
    let mut after = before.clone();
    let mut transactions = Vec::new();
    let mut subscription = subscription(&queue, facts, subscribed);
    let mut tags = tags(&queue, facts, tagged);
    for entry in queue.entries {
        match entry {
            Entry::Subscribers => {
                transactions.extend(subscription.take().map(Planned::Subscription));
            }
            Entry::Projects => transactions.extend(tags.take().map(Planned::Tags)),
            Entry::Reviewers(tokens) => {
                let Some(new) = super::reviewers::herald_value(&before, tokens) else {
                    continue;
                };
                super::reviewers::apply_changes(&mut after, &before, &new);
                transactions.push(Planned::Reviewers(super::reviewers::Planned {
                    phid: super::transaction_phid(),
                    old: before.clone(),
                    new,
                }));
            }
        }
    }
    let reviewers = (after != before)
        .then(|| super::reviewers::apply(revision, &mut facts.reviewers, before, after));
    if reviewers.is_some()
        && super::new_review_status(revision, sticky_accept)
            .is_some_and(|status| status != revision.status)
    {
        return Err(MethodError::Unsupported(
            "review statuses Herald's reviewers change",
        ));
    }
    Ok(Edits {
        transactions,
        reviewers,
        email: queue.email,
        forced_email: queue.forced_email,
    })
}

/// A list of edges as the edge query reads it (newest first) after Herald:
/// the edit's new edges (`main`, numbered in order) and Herald's (`added`)
/// are all as old, so edges of the same number come by PHID, descending,
/// as MySQL reads the `(src, type, dateCreated, seq)` index backwards.
fn newest_first(
    old: &[String],
    main: &[String],
    added: &[String],
    removed: &[String],
) -> Vec<String> {
    let mut fresh: Vec<(usize, &String)> = main
        .iter()
        .filter(|phid| !removed.contains(phid))
        .enumerate()
        .chain(added.iter().enumerate())
        .collect();
    fresh.sort_by(|(left_seq, left), (right_seq, right)| {
        right_seq.cmp(left_seq).then_with(|| right.cmp(left))
    });
    let mut after: Vec<String> = fresh.into_iter().map(|(_, phid)| phid.clone()).collect();
    after.extend(
        old.iter()
            .filter(|phid| !removed.contains(phid) && !main.contains(phid))
            .cloned(),
    );
    after
}

/// The tags' `core:edge` transaction of the queue, if it has an effect, in
/// the slim format of edges. Updates the projects of `facts`.
fn tags(queue: &Queue, facts: &mut Facts, tagged: &[String]) -> Option<super::tags::Planned> {
    let old = facts.projects.clone();
    let mut new = old.clone();
    for phid in &queue.tag {
        if !new.contains(phid) {
            new.push(phid.clone());
        }
    }
    new.retain(|phid| !queue.untag.contains(phid));
    let added = super::lists::minus(&new, &old);
    let removed = super::lists::minus(&old, &new);
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    facts.projects = newest_first(&old, tagged, &added, &removed);
    Some(super::tags::Planned {
        phid: super::transaction_phid(),
        removed,
        added,
    })
}

/// The `core:subscribers` transaction of the queue, if it has an effect:
/// the subscribers before, then `getPHIDList()`. Updates the subscribers
/// of `facts` ([`newest_first()`]).
fn subscription(queue: &Queue, facts: &mut Facts, subscribed: &[String]) -> Option<Subscription> {
    let old = facts.subscribers.clone();
    let mut new = old.clone();
    for phid in &queue.add {
        if !new.contains(phid) {
            new.push(phid.clone());
        }
    }
    new.retain(|phid| !queue.remove.contains(phid));
    if new == old {
        return None;
    }
    let added = super::lists::minus(&new, &old);
    let removed = super::lists::minus(&old, &new);
    facts.subscribers = newest_first(&old, subscribed, &added, &removed);
    Some(Subscription {
        phid: super::transaction_phid(),
        old,
        new,
        first: false,
        added: queue.add.clone(),
    })
}

/// Writes the Herald editor's transactions, which name the transcript, and
/// their effects.
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    transcript_id: u64,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let edits = &plan.herald;
    let metadata = php::json_encode(&json!({
        "herald:transcriptID": transcript_id,
        "core.groupID": plan.group_id,
    }));
    for planned in &edits.transactions {
        let (phid, kind, old, new) = match planned {
            Planned::Subscription(edit) => (
                &edit.phid,
                "core:subscribers",
                php::json_encode(&json!(edit.old)),
                php::json_encode(&json!(edit.new)),
            ),
            Planned::Reviewers(edit) => (
                &edit.phid,
                "differential.revision.reviewers",
                super::reviewers::encode(&edit.old),
                super::reviewers::encode(&edit.new),
            ),
            Planned::Tags(edit) => (
                &edit.phid,
                "core:edge",
                php::json_encode(&json!(edit.removed)),
                php::json_encode(&json!(edit.added)),
            ),
        };
        // Edge transactions name their type first.
        let metadata = if matches!(planned, Planned::Tags(_)) {
            php::json_encode(&json!({
                "edge:type": edges::OBJECT_HAS_PROJECT,
                "herald:transcriptID": transcript_id,
                "core.groupID": plan.group_id,
            }))
        } else {
            metadata.clone()
        };
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
             commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
             dateCreated, dateModified) VALUES (?, ?, ?, 'public', 'no-one', NULL, 0, ?, ?, ?, \
             ?, ?, ?, ?)",
            database.table("differential", "differential_transaction")
        ))
        .bind(phid)
        .bind(HERALD_PHID)
        .bind(plan.revision.phid.as_str())
        .bind(kind)
        .bind(&old)
        .bind(&new)
        .bind(HERALD_SOURCE)
        .bind(&metadata)
        .bind(request.now)
        .bind(request.now)
        .execute(&mut *connection)
        .await?;
    }
    for planned in &edits.transactions {
        match planned {
            Planned::Subscription(edit) => {
                super::write_subscription_edges(request, plan, edit, connection).await?;
            }
            Planned::Tags(edit) => {
                super::tags::write_edges(request, plan, edit, connection).await?;
            }
            Planned::Reviewers(_) => {}
        }
    }
    if let Some(edit) = &edits.reviewers {
        let revision = plan.revision.phid.as_str();
        super::reviewers::write(connection, database, revision, edit, request.now).await?;
    }
    Ok(())
}
