//! `HeraldRuleEditor`: the transactions of global and personal rules, as
//! the web forms apply them. `HeraldDisableController` disables or enables
//! a rule (`herald:disable`); `HeraldRuleController::saveRule()` creates a
//! rule (`core:create`) or edits one, with its conditions, actions and
//! repetition policy (`herald:edit`) and its name (`herald:name`).
//!
//! What the editor writes: the rule (every column, as Lisk saves it), the
//! transactions, the conditions and actions an edit replaces
//! (`saveConditions()`/`saveActions()`), then Herald, which runs on rules
//! (`HeraldRuleAdapter`) with no rule to evaluate (a transcript and the
//! saved header), and the publish task, whose state lists the mail PHP
//! sends (to the actor and a personal rule's author, cc'ing subscribers),
//! without a feed story.
//!
//! Left to PHP ([`MethodError::Unsupported`], before anything is written):
//! object rules, rules written by a newer Herald, edits without effect
//! (which PHP still saves and publishes), and any enabled rule about Herald
//! rules, which the Rust engine doesn't evaluate.

use phab_conduit::edit::{self, Planned, Publication};
use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, PhidType, php};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, DbError, edges, rows, worker};
use serde_json::{Value, json};

use crate::engine::{self, HeraldError, Subject};

/// `HeraldRuleAdapter::getAdapterContentType()`.
const RULE_ADAPTER: &str = "HeraldRuleAdapter";

/// `HeraldRule::$configVersion`, which the rule controller upgrades rules
/// to when it saves them.
pub const CONFIG_VERSION: i64 = 38;

/// `getColumnMaximumByteLength('name')` (`sort255`).
const NAME_LENGTH: usize = 255;

/// `PhabricatorObjectHasSubscriberEdgeType`.
const OBJECT_HAS_SUBSCRIBER: i64 = 21;

/// What an edit without effect is.
const NO_EFFECT: &str = "edits without effect";

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// The rule an edit changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An existing rule, by ID.
    Rule(i64),
    /// A rule the edit creates, for the actor (`HeraldRuleController` with
    /// a content type and a rule type).
    New {
        /// The adapter, like `differential`.
        content_type: String,
        /// `global` or `personal`.
        rule_type: String,
    },
}

/// A condition of a saved rule, as `HeraldRuleSerializer` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionValue {
    /// `fieldName`.
    pub field: String,
    /// `fieldCondition`.
    pub condition: String,
    /// The value, as `willSaveCondition()` leaves it.
    pub value: Value,
}

/// An action of a saved rule.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionValue {
    /// `action`.
    pub action: String,
    /// The target, as `willSaveAction()` leaves it.
    pub target: Value,
}

/// What `HeraldRuleController::saveRule()` saves besides the name
/// (`serializeRuleComponents()`).
#[derive(Debug, Clone, PartialEq)]
pub struct Components {
    /// `match_all`.
    pub match_all: bool,
    /// The conditions, in order.
    pub conditions: Vec<ConditionValue>,
    /// The actions, in order.
    pub actions: Vec<ActionValue>,
    /// `every`, `first` or `change`.
    pub repetition_policy: String,
}

impl Components {
    /// `serializeRuleComponents()`.
    fn serialize(&self) -> Value {
        json!({
            "match_all": self.match_all,
            "conditions": self
                .conditions
                .iter()
                .map(|condition| json!({
                    "field": condition.field,
                    "condition": condition.condition,
                    "value": condition.value,
                }))
                .collect::<Vec<_>>(),
            "actions": self
                .actions
                .iter()
                .map(|action| json!({"action": action.action, "target": action.target}))
                .collect::<Vec<_>>(),
            "repetition_policy": self.repetition_policy,
        })
    }
}

/// The transactions of an edit.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// `herald:disable`: disables (`true`) or enables the rule.
    Disable(bool),
    /// The rule form: `core:create` for a new rule, then `herald:edit` and
    /// `herald:name`.
    Save {
        /// The posted name.
        name: String,
        /// The conditions, actions and repetition policy.
        components: Components,
    },
}

/// The rule saved by an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The rule ID.
    pub id: i64,
    /// The rule PHID.
    pub phid: String,
}

/// The rule as the edit leaves it.
#[derive(Debug, Clone)]
struct State {
    id: Option<i64>,
    phid: String,
    name: Option<String>,
    author: String,
    content_type: String,
    rule_type: String,
    must_match_all: bool,
    config_version: i64,
    repetition_policy: Option<String>,
    is_disabled: bool,
    conditions: Vec<ConditionValue>,
    actions: Vec<ActionValue>,
}

impl State {
    /// `HeraldRuleSerializer::serializeRule()`.
    fn serialize(&self) -> Value {
        json!({
            "match_all": self.must_match_all,
            "conditions": self
                .conditions
                .iter()
                .map(|condition| json!({
                    "field": condition.field,
                    "condition": condition.condition,
                    "value": condition.value,
                }))
                .collect::<Vec<_>>(),
            "actions": self
                .actions
                .iter()
                .map(|action| json!({"action": action.action, "target": action.target}))
                .collect::<Vec<_>>(),
            "repetition_policy": self.repetition_policy,
        })
    }

    fn is_personal(&self) -> bool {
        self.rule_type == "personal"
    }
}

/// The rule, locked for the edit (`beginReadLocking()` and `reload()`),
/// with its conditions and actions (`loadConditions()`, which PHP's
/// controller attaches, in ID order).
async fn load(
    connection: &mut MySqlConnection,
    database: &Database,
    id: i64,
) -> Result<State, MethodError> {
    let row = phab_db::sqlx::query(&format!(
        "SELECT id, phid, name, authorPHID, contentType, mustMatchAll, configVersion, \
         repetitionPolicy, ruleType, isDisabled FROM {} WHERE id = ? FOR UPDATE",
        database.table("herald", "herald_rule")
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(|| unsupported("unknown Herald rules"))?;
    let rule_type = rows::text(&row, "ruleType")?;
    if !matches!(rule_type.as_str(), "global" | "personal") {
        return Err(unsupported("object Herald rules"));
    }
    let config_version = rows::int(&row, "configVersion")?;
    if config_version > CONFIG_VERSION {
        return Err(unsupported("Herald rules of a newer version"));
    }
    let (conditions, actions) = children(connection, database, id).await?;
    Ok(State {
        id: Some(id),
        phid: rows::text(&row, "phid")?,
        name: Some(rows::text(&row, "name")?),
        author: rows::text(&row, "authorPHID")?,
        content_type: rows::text(&row, "contentType")?,
        rule_type,
        must_match_all: rows::flag(&row, "mustMatchAll")?,
        config_version,
        repetition_policy: Some(rows::text(&row, "repetitionPolicy")?),
        is_disabled: rows::int(&row, "isDisabled")? != 0,
        conditions,
        actions,
    })
}

/// `configVersion` of a rule.
///
/// # Errors
///
/// When the query fails.
pub async fn config_version(database: &Database, id: i64) -> Result<Option<i64>, DbError> {
    phab_db::sqlx::query(&format!(
        "SELECT configVersion FROM {} WHERE id = ?",
        database.table("herald", "herald_rule")
    ))
    .bind(id)
    .fetch_optional(database.pool())
    .await?
    .map(|row| rows::int(&row, "configVersion"))
    .transpose()
}

/// The conditions and actions of a rule, as the editor serializes them
/// (`json_decode()` of their columns).
///
/// # Errors
///
/// When a query fails.
pub async fn load_components(
    database: &Database,
    id: i64,
) -> Result<(Vec<ConditionValue>, Vec<ActionValue>), MethodError> {
    let mut connection = database.pool().acquire().await?;
    children(&mut connection, database, id).await
}

/// The rule's conditions and actions, in ID order.
async fn children(
    connection: &mut MySqlConnection,
    database: &Database,
    id: i64,
) -> Result<(Vec<ConditionValue>, Vec<ActionValue>), MethodError> {
    let mut conditions = Vec::new();
    for row in phab_db::sqlx::query(&format!(
        "SELECT fieldName, fieldCondition, value FROM {} WHERE ruleID = ? ORDER BY id",
        database.table("herald", "herald_condition")
    ))
    .bind(id)
    .fetch_all(&mut *connection)
    .await?
    {
        conditions.push(ConditionValue {
            field: rows::text(&row, "fieldName")?,
            condition: rows::text(&row, "fieldCondition")?,
            value: php::round_trip(&rows::json(&row, "value")?),
        });
    }
    let mut actions = Vec::new();
    for row in phab_db::sqlx::query(&format!(
        "SELECT action, target FROM {} WHERE ruleID = ? ORDER BY id",
        database.table("herald", "herald_action")
    ))
    .bind(id)
    .fetch_all(&mut *connection)
    .await?
    {
        actions.push(ActionValue {
            action: rows::text(&row, "action")?,
            target: php::round_trip(&rows::json(&row, "target")?),
        });
    }
    Ok((conditions, actions))
}

/// A new rule (`new HeraldRule()` as the controller sets it up).
fn new_state(actor: &str, content_type: String, rule_type: String) -> State {
    State {
        id: None,
        phid: Phid::generate(PhidType::HERALD_RULE, None).into_string(),
        name: None,
        author: actor.to_owned(),
        content_type,
        rule_type,
        must_match_all: true,
        config_version: CONFIG_VERSION,
        repetition_policy: None,
        is_disabled: false,
        conditions: Vec::new(),
        actions: Vec::new(),
    }
}

/// The errors of `HeraldRuleNameTransaction::validateTransactions()`.
pub fn name_errors(name: &str) -> Vec<String> {
    if name.is_empty() {
        vec!["Rules must have a name.".to_owned()]
    } else if name.len() > NAME_LENGTH {
        vec![format!(
            "Rule names can be no longer than {NAME_LENGTH} characters."
        )]
    } else {
        Vec::new()
    }
}

/// Whether two values are identical (`!==`), as their encodings say.
fn identical(old: &Value, new: &Value) -> bool {
    php::json_encode(old) == php::json_encode(new)
}

/// The transactions of `change` with an effect, applied to `state`
/// (`applyInternalEffects()`); `true` when the conditions and actions
/// change (`herald:edit`'s external effects).
fn plan(state: &mut State, change: Change) -> Result<(Vec<Planned>, bool), MethodError> {
    let rule = PhidType::HERALD_RULE;
    let is_new = state.id.is_none();
    let mut planned = Vec::new();
    let mut replaces_children = false;
    match change {
        Change::Disable(disabled) => {
            let xaction = Planned::new(
                rule,
                "herald:disable",
                (json!(state.is_disabled), json!(disabled)),
            );
            if xaction.has_effect() {
                state.is_disabled = disabled;
                planned.push(xaction);
            }
        }
        Change::Save { name, components } => {
            let errors = name_errors(&name);
            if !errors.is_empty() {
                return Err(edit::validation_error(&errors));
            }
            if is_new {
                planned.push(Planned::new(
                    rule,
                    "core:create",
                    (Value::Null, Value::Null),
                ));
            }
            let (old, new) = (state.serialize(), components.serialize());
            if !identical(&old, &new) {
                planned.push(Planned::new(rule, "herald:edit", (old, new)));
                state.must_match_all = components.match_all;
                state.conditions = components.conditions;
                state.actions = components.actions;
                state.repetition_policy = Some(components.repetition_policy);
                replaces_children = true;
            }
            let old = state.name.as_ref().map_or(Value::Null, |name| json!(name));
            let xaction = Planned::new(rule, "herald:name", (old, json!(name)));
            if xaction.has_effect() {
                state.name = Some(name);
                planned.push(xaction);
            }
            // The controller upgrades the rule it saves.
            state.config_version = CONFIG_VERSION;
        }
    }
    let effective = planned.iter().any(|xaction| xaction.kind != "core:create") || is_new;
    if !effective {
        return Err(unsupported(NO_EFFECT));
    }
    if is_new {
        for xaction in &mut planned {
            xaction
                .metadata
                .insert("core.create".to_owned(), json!(true));
        }
    }
    Ok((planned, replaces_children))
}

/// Saves the rule as Lisk does: every column. Returns the ID.
async fn save_rule(
    connection: &mut MySqlConnection,
    database: &Database,
    state: &State,
    now: i64,
) -> Result<i64, MethodError> {
    let table = database.table("herald", "herald_rule");
    let repetition = state
        .repetition_policy
        .as_deref()
        .ok_or_else(|| unsupported("rules without a repetition policy"))?;
    let name = state.name.as_deref().unwrap_or_default();
    if let Some(id) = state.id {
        phab_db::sqlx::query(&format!(
            "UPDATE {table} SET name = ?, mustMatchAll = ?, configVersion = ?, \
             repetitionPolicy = ?, isDisabled = ?, dateModified = ? WHERE id = ?"
        ))
        .bind(name)
        .bind(i64::from(state.must_match_all))
        .bind(state.config_version)
        .bind(repetition)
        .bind(i64::from(state.is_disabled))
        .bind(now)
        .bind(id)
        .execute(&mut *connection)
        .await?;
        return Ok(id);
    }
    let id = phab_db::sqlx::query(&format!(
        "INSERT INTO {table} (name, authorPHID, contentType, mustMatchAll, configVersion, \
         dateCreated, dateModified, repetitionPolicy, ruleType, phid, isDisabled, \
         triggerObjectPHID) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)"
    ))
    .bind(name)
    .bind(&state.author)
    .bind(&state.content_type)
    .bind(i64::from(state.must_match_all))
    .bind(state.config_version)
    .bind(now)
    .bind(now)
    .bind(repetition)
    .bind(&state.rule_type)
    .bind(&state.phid)
    .bind(i64::from(state.is_disabled))
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    i64::try_from(id).map_err(MethodError::internal)
}

/// `saveConditions()` and `saveActions()`: the rule's children are
/// deleted, then inserted one by one.
async fn save_children(
    connection: &mut MySqlConnection,
    database: &Database,
    (id, state): (i64, &State),
) -> Result<(), MethodError> {
    let conditions = database.table("herald", "herald_condition");
    phab_db::sqlx::query(&format!("DELETE FROM {conditions} WHERE ruleID = ?"))
        .bind(id)
        .execute(&mut *connection)
        .await?;
    for condition in &state.conditions {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {conditions} (ruleID, fieldName, fieldCondition, value) VALUES (?, ?, \
             ?, ?)"
        ))
        .bind(id)
        .bind(&condition.field)
        .bind(&condition.condition)
        .bind(php::json_encode(&condition.value))
        .execute(&mut *connection)
        .await?;
    }
    let actions = database.table("herald", "herald_action");
    phab_db::sqlx::query(&format!("DELETE FROM {actions} WHERE ruleID = ?"))
        .bind(id)
        .execute(&mut *connection)
        .await?;
    for action in &state.actions {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {actions} (ruleID, action, target) VALUES (?, ?, ?)"
        ))
        .bind(id)
        .bind(&action.action)
        .bind(php::json_encode(&action.target))
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// The destinations of the rule's edges of a type, newest first.
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

/// The `state` of the publish task (`getWorkerState()`): mail to the actor
/// and a personal rule's author (`getMailTo()`), cc'ing the subscribers,
/// and no feed story.
fn worker_state(
    state: &State,
    (id, actor, is_new): (i64, &str, bool),
    (header, subscribers, muted): (&str, &[String], &[String]),
) -> Value {
    let mut to = vec![actor.to_owned()];
    if state.is_personal() {
        to.push(state.author.clone());
    }
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    let stamps = vec![
        stamp("string", "application", json!("Herald")),
        stamp("string", "monogram", json!(format!("H{id}"))),
        stamp("string", "phid", json!(state.phid)),
        stamp("string", "object-type", json!("HRUL")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!("web")),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!([])),
        stamp("bool", "new", json!(is_new)),
        stamp("phid", "mention", json!([])),
        stamp("string", "herald", engine::rule_monograms(header)),
        stamp("phid", "removed", json!([])),
        stamp("phid", "subscriber", json!(subscribers)),
    ];
    json!({
        "parentMessageID": null,
        "isNewObject": is_new,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": header,
        "mailToPHIDs": to,
        "mailCCPHIDs": subscribers,
        "feedNotifyPHIDs": [],
        "feedRelatedPHIDs": [],
        "feedShouldPublish": false,
        "mailShouldSend": true,
        "mustEncrypt": [],
        "mailStamps": stamps,
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": muted,
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": [],
        "custom.encoding": [],
    })
}

/// Herald on the rule (`HeraldRuleAdapter`), with no rule to evaluate:
/// the transcript and the saved header, which it returns.
async fn write_herald(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    (id, phid, applied): (i64, &str, Vec<String>),
    saved: Option<&str>,
) -> Result<String, MethodError> {
    let database = &request.context.database;
    let subject = Subject {
        phid: phid.to_owned(),
        content_type: RULE_ADAPTER.to_owned(),
        name: format!("H{id}"),
        applied_transaction_phids: Some(applied),
    };
    let evaluation = engine::evaluate(&[], &subject, &[]);
    let transcript = evaluation.transcript(&subject, &[]);
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
    let header = engine::combine_headers(saved, &engine::rules_header(&[]));
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, header) VALUES (?, ?) ON DUPLICATE KEY UPDATE header = \
         VALUES(header)",
        database.table("herald", "herald_savedheader")
    ))
    .bind(phid)
    .bind(&header)
    .execute(&mut *connection)
    .await?;
    Ok(header)
}

/// Applies `change` to `target` as `HeraldRuleEditor` does
/// (`setContinueOnNoEffect(true)`), as the viewer, with the content
/// source `source` (`phab_db::editor::WEB_SOURCE` for the web forms).
/// The caller checked that the viewer may edit the rule.
///
/// # Errors
///
/// [`MethodError::Unsupported`], before anything is written, for what is
/// left to PHP, and the validation errors of the name, as
/// `PhabricatorApplicationTransactionValidationException` words them.
pub async fn apply(
    request: &ApiRequest<'_>,
    target: Target,
    change: Change,
    source: &'static str,
) -> Result<Outcome, MethodError> {
    edit::require_writes(request)?;
    let actor = edit::actor(request)?;
    let database = &request.context.database;
    let about_rules =
        engine::load_rules(database, RULE_ADAPTER)
            .await
            .map_err(|error| match error {
                HeraldError::Unsupported(what) => MethodError::Unsupported(what),
                other => MethodError::internal(other),
            })?;
    if !about_rules.is_empty() {
        return Err(unsupported("Herald rules about Herald rules"));
    }

    let now = request.now;
    let mut transaction = database.pool().begin().await?;
    let mut state = match target {
        Target::Rule(id) => load(&mut transaction, database, id).await?,
        Target::New {
            content_type,
            rule_type,
        } => {
            if !matches!(rule_type.as_str(), "global" | "personal") {
                return Err(unsupported("object Herald rules"));
            }
            new_state(&actor, content_type, rule_type)
        }
    };
    let is_new = state.id.is_none();
    let (planned, replaces_children) = plan(&mut state, change)?;
    let saved = if is_new {
        None
    } else {
        let sql = format!(
            "SELECT header FROM {} WHERE phid = ?",
            database.table("herald", "herald_savedheader")
        );
        phab_db::sqlx::query(&sql)
            .bind(&state.phid)
            .fetch_optional(&mut *transaction)
            .await?
            .map(|row| rows::text(&row, "header"))
            .transpose()?
    };
    let subscribers = if is_new {
        Vec::new()
    } else {
        destinations(database, &state.phid, OBJECT_HAS_SUBSCRIBER).await?
    };
    let muted = if is_new {
        Vec::new()
    } else {
        destinations(database, &state.phid, edges::MUTED_BY).await?
    };

    let id = save_rule(&mut transaction, database, &state, now).await?;
    let mut publication =
        Publication::new(database, actor.clone(), state.phid.clone(), planned).await?;
    publication.content_source = source;
    let table = database.table("herald", "herald_ruletransaction");
    publication
        .write_transactions(&mut transaction, &table, now)
        .await?;
    if replaces_children {
        save_children(&mut transaction, database, (id, &state)).await?;
    }
    let applied = publication.transaction_phids();
    let header = write_herald(
        request,
        &mut transaction,
        (id, &state.phid, applied.clone()),
        saved.as_deref(),
    )
    .await?;
    let data = php::json_encode(&json!({
        "objectPHID": state.phid,
        "actorPHID": actor,
        "xactionPHIDs": applied,
        "state": worker_state(&state, (id, &actor, is_new), (&header, &subscribers, &muted)),
    }));
    let task = worker::Task {
        class: "PhabricatorApplicationTransactionPublishWorker",
        data: &data,
        priority: worker::PRIORITY_ALERTS,
        object_phid: Some(&state.phid),
    };
    worker::schedule_task(&mut transaction, database, task, now).await?;
    transaction.commit().await?;
    Ok(Outcome {
        id,
        phid: state.phid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> State {
        State {
            id: Some(5),
            phid: "PHID-HRUL-5".to_owned(),
            name: Some("Tag".to_owned()),
            author: "PHID-USER-alice".to_owned(),
            content_type: "differential".to_owned(),
            rule_type: "personal".to_owned(),
            must_match_all: true,
            config_version: 37,
            repetition_policy: Some("every".to_owned()),
            is_disabled: false,
            conditions: vec![ConditionValue {
                field: "differential.revision.title".to_owned(),
                condition: "contains".to_owned(),
                value: json!("x"),
            }],
            actions: Vec::new(),
        }
    }

    #[test]
    fn validates_names_like_php() {
        assert_eq!(name_errors(""), ["Rules must have a name."]);
        assert_eq!(
            name_errors(&"x".repeat(256)),
            ["Rule names can be no longer than 255 characters."]
        );
        assert_eq!(name_errors(&"x".repeat(255)), Vec::<String>::new());
    }

    #[test]
    fn plans_like_the_editor() {
        let mut state = rule();
        assert_eq!(
            plan(&mut state, Change::Disable(false)),
            Err(MethodError::Unsupported(NO_EFFECT))
        );
        let (planned, children) = plan(&mut state, Change::Disable(true)).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(
            (planned[0].old.clone(), planned[0].new.clone()),
            (json!(false), json!(true))
        );
        assert!(!children);
        assert!(state.is_disabled);

        // Only the name changes: the edit transaction has no effect.
        let mut state = rule();
        let components = Components {
            match_all: true,
            conditions: state.conditions.clone(),
            actions: Vec::new(),
            repetition_policy: "every".to_owned(),
        };
        let change = Change::Save {
            name: "Tag it".to_owned(),
            components: components.clone(),
        };
        let (planned, children) = plan(&mut state, change).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].kind, "herald:name");
        assert!(!children);
        assert_eq!(state.config_version, CONFIG_VERSION);

        // A new rule: created, edited and named.
        let mut state = new_state(
            "PHID-USER-alice",
            "differential".to_owned(),
            "global".to_owned(),
        );
        let change = Change::Save {
            name: "New".to_owned(),
            components,
        };
        let (planned, children) = plan(&mut state, change).unwrap();
        let kinds: Vec<&str> = planned
            .iter()
            .map(|xaction| xaction.kind.as_str())
            .collect();
        assert_eq!(kinds, ["core:create", "herald:edit", "herald:name"]);
        assert!(children);
        assert_eq!(
            php::json_encode(&planned[1].old),
            r#"{"match_all":true,"conditions":[],"actions":[],"repetition_policy":null}"#
        );
        assert_eq!(planned[2].old, Value::Null);
        assert!(
            planned
                .iter()
                .all(|xaction| xaction.metadata["core.create"] == json!(true))
        );
    }

    #[test]
    fn stamps_the_mail_like_php() {
        let state = rule();
        let subscribers = vec!["PHID-USER-bob".to_owned()];
        let value = worker_state(
            &state,
            (5, "PHID-USER-alice", false),
            ("<3>", &subscribers, &[]),
        );
        assert_eq!(
            value["mailToPHIDs"],
            json!(["PHID-USER-alice", "PHID-USER-alice"])
        );
        assert_eq!(value["mailCCPHIDs"], json!(["PHID-USER-bob"]));
        assert_eq!(value["mailStamps"][10]["value"], json!(["H3"]));
        assert_eq!(value["mailStamps"][12]["key"], json!("subscriber"));
    }
}
