//! The actions of Herald rules which edit the revision
//! (`PhabricatorSubscriptionsHeraldAction`): each logs what it did
//! (`logEffect()`, kept in the transcript) and queues a transaction, which
//! PHP's Herald editor applies after the edit's, as the Herald
//! application (`applyHeraldRules()`).

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, php};
use phab_db::phpser::{Key, Value as Php};
use phab_db::{edges, rows};
use serde_json::json;

use super::{Facts, Plan, Subscription};
use crate::storage::Revision;

/// `PhabricatorHeraldApplication`'s PHID, the author of its transactions.
const HERALD_PHID: &str = "PHID-APPS-PhabricatorHeraldApplication";

/// `PhabricatorHeraldContentSource`.
const HERALD_SOURCE: &str = r#"{"source":"herald","params":[]}"#;

/// The subscriptions the actions queued: their `+` and `-`, merged as
/// `combineTransactions()` merges them (a later transaction's PHIDs first).
#[derive(Default)]
pub(super) struct Queue {
    add: Vec<String>,
    remove: Vec<String>,
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

/// `loadStandardTargets()` for users and projects: the targets which can
/// be acted on, and the log of those which can't.
async fn standard_targets(
    request: &ApiRequest<'_>,
    revision: &Revision,
    phids: Vec<String>,
    log: &mut Vec<(String, Php)>,
) -> Result<Vec<String>, MethodError> {
    if phids.is_empty() {
        log.push(("do.standard.empty".to_owned(), Php::Null));
        return Ok(Vec::new());
    }
    let (valid, invalid): (Vec<String>, Vec<String>) = phids
        .into_iter()
        .partition(|phid| phid.starts_with("PHID-USER-") || phid.starts_with("PHID-PROJ-"));
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
    let phids: Vec<String> = match targets {
        Php::Array(entries) => entries
            .iter()
            .filter_map(|(_, value)| value.as_bytes())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect(),
        _ => Vec::new(),
    };
    // PhabricatorObjectQuery orders several targets by type and query.
    if phids.len() > 1 {
        return Err(MethodError::Unsupported(
            "Herald actions on several targets",
        ));
    }
    let mut phids = standard_targets(request, revision, phids, &mut log).await?;
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
    let into = if is_add {
        &mut queue.add
    } else {
        &mut queue.remove
    };
    let mut merged = remaining.clone();
    merged.extend(
        into.iter()
            .filter(|phid| !remaining.contains(phid))
            .cloned(),
    );
    *into = merged;
    let code = if is_add {
        "do.subscribed"
    } else {
        "do.unsubscribed"
    };
    log.push((code.to_owned(), keyed(&remaining)));
    Ok(log)
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

/// The Herald editor's `core:subscribers` transaction of the queue, if it
/// has an effect: the subscribers before, then `getPHIDList()`. Updates the
/// subscribers of `facts`, as the edge query reads them (newest first):
/// the edit's new edges (`subscribed`, numbered in order) and Herald's are
/// all as old, so edges of the same number come by PHID, descending, as
/// MySQL reads the `(src, type, dateCreated, seq)` index backwards.
pub(super) fn subscription(
    queue: &Queue,
    facts: &mut Facts,
    subscribed: &[String],
) -> Option<Subscription> {
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
    let mut fresh: Vec<(usize, &String)> = subscribed
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
            .filter(|phid| !removed.contains(phid) && !subscribed.contains(phid))
            .cloned(),
    );
    facts.subscribers = after;
    Some(Subscription {
        phid: super::transaction_phid(),
        old,
        new,
        first: false,
        added: queue.add.clone(),
    })
}

/// Writes the Herald editor's transactions, which name the transcript.
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    transcript_id: u64,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let Some(subscription) = &plan.herald_subscription else {
        return Ok(());
    };
    let database = &request.context.database;
    let metadata = php::json_encode(&json!({
        "herald:transcriptID": transcript_id,
        "core.groupID": plan.group_id,
    }));
    let (old, new) = (
        php::json_encode(&json!(subscription.old)),
        php::json_encode(&json!(subscription.new)),
    );
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
         commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
         dateCreated, dateModified) VALUES (?, ?, ?, 'public', 'no-one', NULL, 0, \
         'core:subscribers', ?, ?, ?, ?, ?, ?)",
        database.table("differential", "differential_transaction")
    ))
    .bind(&subscription.phid)
    .bind(HERALD_PHID)
    .bind(plan.revision.phid.as_str())
    .bind(&old)
    .bind(&new)
    .bind(HERALD_SOURCE)
    .bind(&metadata)
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *connection)
    .await?;
    super::write_subscription_edges(request, plan, subscription, connection).await
}
