//! `differential.revision.edit` (`DifferentialRevisionEditEngine` through
//! `PhabricatorEditEngine::buildConduitResponse()`): the transaction
//! editor, for comments.
//!
//! An edit is planned first: every read, check and Herald evaluation, and
//! the state the publish worker gets. Anything not ported is
//! [`MethodError::Unsupported`] at that point, before anything is written.
//! Then one MySQL transaction writes the rows PHP's editor would, and
//! queues PHP's `PhabricatorApplicationTransactionPublishWorker`, so mail,
//! feed stories and search indexing still come from PHP.
//!
//! Ported: one `comment` on an existing revision, without remarkup that
//! mentions or references anything, by an actor who can see it, when
//! Herald's rules are ones [`phab_app_herald::engine`] evaluates and the
//! review status stays the same. A comment subscribes its author when
//! nothing else does (`applyImplicitCC()`).

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
        .any(|character| matches!(character, '@' | '{' | '#' | '[' | '<' | '/' | ':'))
    {
        return true;
    }
    // D123, T4, rXYZabc, F1, H2, and the like.
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| word.bytes().any(|byte| byte.is_ascii_digit()))
}

/// `isEmptyComment()`.
fn is_empty_comment(text: &str) -> bool {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
        .is_empty()
}

/// The comment of the `transactions` parameter.
fn comment(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    let Some(Value::Array(transactions)) = request.value("transactions") else {
        return Err(MethodError::Unsupported("unusual transaction lists"));
    };
    let [transaction] = transactions.as_slice() else {
        return Err(MethodError::Unsupported("several transactions"));
    };
    if transaction.get("type").and_then(Value::as_str) != Some("comment") {
        return Err(MethodError::Unsupported("transactions other than comments"));
    }
    let Some(Value::String(text)) = transaction.get("value") else {
        return Err(MethodError::Unsupported("unusual comments"));
    };
    if is_empty_comment(text) || has_references(text) {
        return Err(MethodError::Unsupported(
            "empty comments and comments with references",
        ));
    }
    Ok(text.clone())
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
            should_broadcast,
            reviewers,
            projects: destinations(database, &revision.phid, edges::OBJECT_HAS_PROJECT).await?,
            subscribers: destinations(database, &revision.phid, edges::OBJECT_HAS_SUBSCRIBER)
                .await?,
            muted: destinations(database, &revision.phid, edges::MUTED_BY).await?,
        })
    }
}

/// Edits PHP's editor makes which Rust leaves to it: a review status
/// change, draft inline comments to publish, build plans, and mail to
/// project watchers.
async fn check_side_effects(
    request: &ApiRequest<'_>,
    revision: &Revision,
    facts: &Facts,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    // `differential.sticky-accept` defaults to true.
    let sticky = request
        .context
        .config
        .value("differential.sticky-accept")
        .is_none_or(php::is_truthy);
    if new_review_status(revision, sticky).is_some_and(|status| status != revision.status) {
        return Err(MethodError::Unsupported(
            "comments which change the review status",
        ));
    }
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

/// The planned edit.
struct Plan {
    revision: Revision,
    text: String,
    actor: String,
    /// The `core:subscribers` transaction `applyImplicitCC()` adds: its
    /// PHID and the subscribers before.
    subscription: Option<(String, Vec<String>)>,
    transaction_phid: String,
    comment_phid: String,
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
        stamp("phid", "mention", json!([])),
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

async fn plan(request: &ApiRequest<'_>) -> Result<Plan, MethodError> {
    let text = comment(request)?;
    let revision = load_revision(request).await?;
    let actor = request
        .viewer
        .phid
        .as_ref()
        .ok_or(MethodError::Unsupported("edits by the omnipotent user"))?
        .as_str()
        .to_owned();
    let database = &request.context.database;
    let facts = Facts::load(database, &revision).await?;
    check_side_effects(request, &revision, &facts).await?;
    let mut facts = facts;
    let subscription = if implies_subscription(database, &revision, &facts, &actor).await? {
        let old = facts.subscribers.clone();
        // The new edge is the newest: it comes first after the edit.
        facts.subscribers.insert(0, actor.clone());
        Some((
            Phid::generate(PhidType::TRANSACTION, Some(PhidType::REVISION)).into_string(),
            old,
        ))
    } else {
        None
    };
    let transaction_phid =
        Phid::generate(PhidType::TRANSACTION, Some(PhidType::REVISION)).into_string();
    // The subscription comes first (array_unshift()).
    let applied_phids: Vec<String> = subscription
        .iter()
        .map(|(phid, _)| phid.clone())
        .chain([transaction_phid.clone()])
        .collect();
    let (rules, evaluation, applied, must_encrypt) =
        run_herald(database, &revision, &facts, &applied_phids).await?;
    let subject = Subject {
        phid: revision.phid.as_str().to_owned(),
        content_type: "differential".to_owned(),
        name: revision.title.clone(),
        applied_transaction_phids: Some(applied_phids.clone()),
    };
    let transcript = evaluation.transcript(&subject, &applied);
    let herald_header = engine::combine_headers(
        saved_header(database, &revision.phid).await?.as_deref(),
        &engine::rules_header(&applied),
    );
    let state = worker_state(&revision, &facts, &actor, &must_encrypt, &herald_header);
    let worker_data = php::json_encode(&json!({
        "objectPHID": revision.phid.as_str(),
        "actorPHID": actor,
        "xactionPHIDs": applied_phids,
        "state": state,
    }));
    Ok(Plan {
        text,
        actor,
        comment_phid: Phid::generate(PhidType::TRANSACTION_COMMENT, None).into_string(),
        group_id: random::random_characters(32),
        transaction_phid,
        subscription,
        rules,
        transcript,
        herald_header,
        worker_data,
        revision,
    })
}

/// Writes the planned edit in one transaction.
async fn write(request: &ApiRequest<'_>, plan: &Plan) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    let revision = &plan.revision;
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET dateModified = ? WHERE id = ?",
        database.table("differential", "differential_revision")
    ))
    .bind(now)
    .bind(revision.id)
    .execute(&mut *connection)
    .await?;
    let metadata = php::json_encode(&json!({"core.groupID": plan.group_id}));
    if let Some((phid, old)) = &plan.subscription {
        write_subscription(request, plan, phid, old, &metadata, connection).await?;
    }
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
         commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
         dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, ?, 1, 'core:comment', 'null', \
         'null', ?, ?, ?, ?)",
        database.table("differential", "differential_transaction")
    ))
    .bind(&plan.transaction_phid)
    .bind(&plan.actor)
    .bind(revision.phid.as_str())
    .bind(&plan.actor)
    .bind(&plan.comment_phid)
    .bind(CONDUIT_SOURCE)
    .bind(&metadata)
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
    .bind(&plan.comment_phid)
    .bind(&plan.transaction_phid)
    .bind(&plan.actor)
    .bind(&plan.actor)
    .bind(&plan.text)
    .bind(CONDUIT_SOURCE)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    // markReviewerComments().
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
    write_herald(request, plan, connection).await?;
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "PhabricatorApplicationTransactionPublishWorker",
            data: &plan.worker_data,
            priority: worker::PRIORITY_ALERTS,
            object_phid: Some(revision.phid.as_str()),
        },
        now,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// The `core:subscribers` transaction of `applyImplicitCC()` and its edges
/// (`PhabricatorEdgeEditor`, with the inverse edge).
async fn write_subscription(
    request: &ApiRequest<'_>,
    plan: &Plan,
    phid: &str,
    old: &[String],
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.phid.as_str();
    let mut new = old.to_vec();
    new.push(plan.actor.clone());
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
    for (application, src, edge_type, dst) in [
        (
            "differential",
            revision,
            edges::OBJECT_HAS_SUBSCRIBER,
            plan.actor.as_str(),
        ),
        (
            "user",
            plan.actor.as_str(),
            edges::SUBSCRIBED_TO_OBJECT,
            revision,
        ),
    ] {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (src, type, dst, dateCreated, seq, dataID) VALUES (?, ?, ?, ?, 0, \
             NULL) ON DUPLICATE KEY UPDATE dataID = VALUES(dataID)",
            database.table(application, "edge")
        ))
        .bind(src)
        .bind(edge_type)
        .bind(dst)
        .bind(request.now)
        .execute(&mut *connection)
        .await?;
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
                    .subscription
                    .iter()
                    .map(|(phid, _)| phid.as_str())
                    .chain([plan.transaction_phid.as_str()])
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
        assert!(has_references("See D123."));
        assert!(has_references("Thanks @alice"));
        assert!(has_references("{F1}"));
        assert!(has_references("https://example.com"));
    }

    #[test]
    fn rule_monograms_like_php() {
        assert_eq!(rule_monograms("<1>, <2>"), json!(["H1", "H2"]));
        assert_eq!(rule_monograms("none"), json!([]));
    }
}
