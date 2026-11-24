//! What the Herald pages read, without changing anything: rules with their
//! conditions and actions (`HeraldRuleQuery`), their subscribers, the
//! policy of global rules, and transcripts (`HeraldTranscriptQuery`)
//! decoded from PHP's serialization.

use phab_core::{Config, Phid};
use phab_db::phpser::{self, Key, Value as Php};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};

use crate::engine::{ActionRecord, Condition};

/// `PhabricatorObjectHasSubscriberEdgeType::EDGECONST`.
const OBJECT_HAS_SUBSCRIBER: i64 = 21;

/// `HeraldManageGlobalRulesCapability::CAPABILITY`.
pub const MANAGE_GLOBAL_RULES: &str = "herald.global";

/// A `herald_rule` row, as the rule pages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRow {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// Who wrote it.
    pub author_phid: Phid,
    /// The adapter, like `differential`.
    pub content_type: String,
    /// Whether every condition must match.
    pub must_match_all: bool,
    /// `every`, `first` or `change`.
    pub repetition_policy: String,
    /// `global`, `personal` or `object`.
    pub rule_type: String,
    /// `isDisabled`.
    pub is_disabled: bool,
    /// The project or repository of an object rule.
    pub trigger_object_phid: Option<Phid>,
}

impl RuleRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            author_phid: Phid::new(rows::text(row, "authorPHID")?),
            content_type: rows::text(row, "contentType")?,
            must_match_all: rows::flag(row, "mustMatchAll")?,
            repetition_policy: rows::text(row, "repetitionPolicy")?,
            rule_type: rows::text(row, "ruleType")?,
            is_disabled: rows::flag(row, "isDisabled")?,
            trigger_object_phid: rows::opt_phid(row, "triggerObjectPHID")?,
        })
    }

    /// `getMonogram()`.
    pub fn monogram(&self) -> String {
        format!("H{}", self.id)
    }

    /// `isObjectRule()`.
    pub fn is_object_rule(&self) -> bool {
        self.rule_type == "object"
    }

    /// The rule as [`crate::visible()`] takes it.
    pub fn summary(&self) -> crate::Rule {
        crate::Rule {
            id: self.id,
            phid: self.phid.clone(),
            name: self.name.clone(),
            content_type: self.content_type.clone(),
            rule_type: self.rule_type.clone(),
            is_disabled: self.is_disabled,
            trigger_object_phid: self.trigger_object_phid.clone(),
        }
    }
}

/// Which rules [`load_rules()`] reads; `None` doesn't constrain.
#[derive(Debug, Clone, Default)]
pub struct RuleFilter<'a> {
    /// `withIDs()`.
    pub ids: Option<&'a [i64]>,
    /// `withPHIDs()`.
    pub phids: Option<&'a [Phid]>,
    /// `withAuthorPHIDs()`.
    pub authors: Option<&'a [Phid]>,
    /// `withDisabled()`.
    pub disabled: Option<bool>,
}

/// The rules matching `filter`, newest first.
pub async fn load_rules(
    database: &Database,
    filter: &RuleFilter<'_>,
) -> Result<Vec<RuleRow>, DbError> {
    let mut clauses = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    for (column, values) in [
        (
            "id",
            filter
                .ids
                .map(|ids| ids.iter().map(ToString::to_string).collect::<Vec<_>>()),
        ),
        (
            "phid",
            filter
                .phids
                .map(|phids| phids.iter().map(|phid| phid.as_str().to_owned()).collect()),
        ),
        (
            "authorPHID",
            filter
                .authors
                .map(|phids| phids.iter().map(|phid| phid.as_str().to_owned()).collect()),
        ),
    ] {
        if let Some(values) = values {
            if values.is_empty() {
                return Ok(Vec::new());
            }
            clauses.push(format!(
                "{column} IN ({})",
                phab_db::placeholders(values.len())
            ));
            binds.extend(values);
        }
    }
    if let Some(disabled) = filter.disabled {
        clauses.push(format!("isDisabled = {}", i32::from(disabled)));
    }
    let mut sql = format!(
        "SELECT id, phid, name, authorPHID, contentType, mustMatchAll, repetitionPolicy, \
         ruleType, isDisabled, triggerObjectPHID FROM {}",
        database.table("herald", "herald_rule"),
    );
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY id DESC");
    phab_db::bind_all(phab_db::sqlx::query(&sql), binds.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(RuleRow::from_row)
        .collect()
}

/// The conditions and actions of a rule, in ID order.
pub async fn load_conditions_and_actions(
    database: &Database,
    rule_id: i64,
) -> Result<(Vec<Condition>, Vec<ActionRecord>), DbError> {
    let sql = format!(
        "SELECT id, fieldName, fieldCondition, value FROM {} WHERE ruleID = ? ORDER BY id",
        database.table("herald", "herald_condition"),
    );
    let mut conditions = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .bind(rule_id)
        .fetch_all(database.pool())
        .await?
    {
        conditions.push(Condition {
            id: rows::int(&row, "id")?.to_string(),
            field: rows::text(&row, "fieldName")?,
            condition: rows::text(&row, "fieldCondition")?,
            value: phpser::from_json(&rows::json(&row, "value")?),
        });
    }
    let sql = format!(
        "SELECT action, target FROM {} WHERE ruleID = ? ORDER BY id",
        database.table("herald", "herald_action"),
    );
    let mut actions = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .bind(rule_id)
        .fetch_all(database.pool())
        .await?
    {
        actions.push(ActionRecord {
            action: rows::text(&row, "action")?,
            target: phpser::from_json(&rows::json(&row, "target")?),
        });
    }
    Ok((conditions, actions))
}

/// `PhabricatorSubscribersQuery::loadSubscribersForPHID()` of a rule,
/// newest first like the edge query.
pub async fn subscribers(database: &Database, rule: &Phid) -> Result<Vec<Phid>, DbError> {
    let sql = format!(
        "SELECT dst FROM {} WHERE src = ? AND type = ? ORDER BY dateCreated DESC, seq DESC",
        database.table("herald", "edge"),
    );
    phab_db::sqlx::query(&sql)
        .bind(rule.as_str())
        .bind(OBJECT_HAS_SUBSCRIBER)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| Ok(Phid::new(rows::text(row, "dst")?)))
        .collect()
}

/// The policy of `HeraldManageGlobalRulesCapability`, who may edit global
/// rules: locked or configured, administrators by default.
pub fn global_rules_policy(config: &Config) -> String {
    if let Some(policy) = config
        .object("policy.locked")
        .and_then(|locked| locked.get(MANAGE_GLOBAL_RULES))
        .and_then(serde_json::Value::as_str)
    {
        return policy.to_owned();
    }
    config
        .object("phabricator.application-settings")
        .and_then(|settings| settings.get("PHID-APPS-PhabricatorHeraldApplication"))
        .and_then(|settings| settings.get("policy"))
        .and_then(|policies| policies.get(MANAGE_GLOBAL_RULES))
        .and_then(serde_json::Value::as_str)
        .filter(|policy| !policy.is_empty())
        .unwrap_or(phab_policy::POLICY_ADMIN)
        .to_owned()
}

/// A `herald_transcript` row without its details.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRow {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The object the rules ran on; empty for some old transcripts.
    pub object_phid: String,
    /// When the rules ran.
    pub time: i64,
    /// How long they took, in seconds.
    pub duration: f64,
    /// Whether it was a test console run.
    pub dry_run: bool,
    /// Where they ran.
    pub host: String,
}

impl TranscriptRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            object_phid: rows::opt_text(row, "objectPHID")?.unwrap_or_default(),
            time: rows::int(row, "time")?,
            duration: phab_db::sqlx::Row::try_get(row, "duration").map_err(DbError::from)?,
            dry_run: rows::flag(row, "dryRun")?,
            host: rows::opt_text(row, "host")?.unwrap_or_default(),
        })
    }
}

/// Up to `limit` transcripts with IDs below `before`, newest first.
pub async fn transcripts(
    database: &Database,
    before: Option<i64>,
    limit: usize,
) -> Result<Vec<TranscriptRow>, DbError> {
    let sql = format!(
        "SELECT id, phid, objectPHID, time, duration, dryRun, host FROM {} WHERE id < ? \
         ORDER BY id DESC LIMIT {limit}",
        database.table("herald", "herald_transcript"),
    );
    phab_db::sqlx::query(&sql)
        .bind(before.unwrap_or(i64::MAX))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(TranscriptRow::from_row)
        .collect()
}

/// A `herald_webhook` row (`HeraldWebhook`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRow {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// Where requests go.
    pub webhook_uri: String,
    /// The view policy; webhooks have no automatic capability.
    pub view_policy: String,
    /// `firehose`, `enabled` or `disabled`.
    pub status: String,
}

/// The webhooks with one of `statuses` (all of them when `None`) and an
/// ID below `before`, newest first (`HeraldWebhookQuery`).
pub async fn webhooks(
    database: &Database,
    statuses: Option<&[&str]>,
    before: Option<i64>,
    limit: usize,
) -> Result<Vec<WebhookRow>, DbError> {
    let status_clause = statuses
        .map(|statuses| format!(" AND status IN ({})", phab_db::placeholders(statuses.len())))
        .unwrap_or_default();
    let sql = format!(
        "SELECT id, phid, name, webhookURI, viewPolicy, status FROM {} WHERE id < ?{status_clause} \
         ORDER BY id DESC LIMIT {limit}",
        database.table("herald", "herald_webhook"),
    );
    let query = phab_db::sqlx::query(&sql).bind(before.unwrap_or(i64::MAX));
    phab_db::bind_all(query, statuses.unwrap_or_default().iter().copied())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok(WebhookRow {
                id: rows::int(row, "id")?,
                phid: Phid::new(rows::text(row, "phid")?),
                name: rows::text(row, "name")?,
                webhook_uri: rows::text(row, "webhookURI")?,
                view_policy: rows::text(row, "viewPolicy")?,
                status: rows::text(row, "status")?,
            })
        })
        .collect()
}

/// A transcript with its details, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    /// The row.
    pub row: TranscriptRow,
    /// `getObjectTranscript()`: `None` once garbage collected.
    pub object: Option<ObjectTranscript>,
    /// `getRuleTranscripts()`.
    pub rules: Vec<RuleTranscript>,
    /// `getConditionTranscripts()`, by rule ID.
    pub conditions: Vec<(String, Vec<ConditionTranscript>)>,
    /// `getApplyTranscripts()`.
    pub applied: Vec<ApplyTranscript>,
}

impl Transcript {
    /// `getConditionTranscriptsForRule()`.
    pub fn conditions_of(&self, rule_id: &str) -> &[ConditionTranscript] {
        self.conditions
            .iter()
            .find(|(id, _)| id == rule_id)
            .map_or(&[], |(_, conditions)| conditions.as_slice())
    }
}

/// A profiler frame of an object transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// `rule` or `field`.
    pub kind: String,
    /// The rule PHID or the field key.
    pub key: String,
    /// Seconds spent.
    pub elapsed: f64,
    /// How many times.
    pub count: i64,
}

/// `HeraldObjectTranscript`.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectTranscript {
    /// The object.
    pub phid: String,
    /// The adapter's content type.
    pub content_type: String,
    /// `getHeraldName()`.
    pub name: String,
    /// The field values the rules read.
    pub fields: Vec<(String, Php)>,
    /// `null` for older transcripts and adapters without transactions.
    pub applied_transaction_phids: Option<Vec<String>>,
    /// `null` for older transcripts.
    pub profile: Option<Vec<Frame>>,
}

/// A `HeraldRuleResult` or `HeraldConditionResult`: its code and data.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultMap {
    /// The result code, like `any-match`.
    pub code: String,
    /// Its data.
    pub data: Php,
}

/// `HeraldRuleTranscript`.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleTranscript {
    /// The rule ID, as a string.
    pub rule_id: String,
    /// The rule's name when it ran.
    pub name: String,
    /// The rule's author.
    pub owner: String,
    /// `getRuleResult()`.
    pub result: ResultMap,
}

/// `HeraldConditionTranscript`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionTranscript {
    /// The field.
    pub field: String,
    /// The condition.
    pub condition: String,
    /// The value tested.
    pub test_value: Php,
    /// `getResult()`.
    pub result: ResultMap,
}

/// `HeraldApplyTranscript`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyTranscript {
    /// The action.
    pub action: String,
    /// Its target.
    pub target: Php,
    /// The rule ID, as a string.
    pub rule_id: String,
    /// `getAppliedReason()`: a list of `{type, data}` maps, or a string in
    /// older transcripts.
    pub applied_reason: Php,
}

/// Why a transcript could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    /// Reading the row failed.
    #[error(transparent)]
    Database(#[from] DbError),
    /// A column is not what PHP writes.
    #[error("transcript {0} has an unexpected format")]
    Format(i64),
}

/// The property `name` of an object, whatever its visibility.
fn property<'a>(object: &'a Php, name: &str) -> Option<&'a Php> {
    let Php::Object { properties, .. } = object else {
        return None;
    };
    properties
        .iter()
        .find(|(mangled, _)| {
            mangled.as_slice() == name.as_bytes()
                || mangled.ends_with(&[b"\0", name.as_bytes()].concat())
        })
        .map(|(_, value)| value)
}

/// `(string)` of a scalar property; `None` for arrays and objects.
pub fn php_string(value: &Php) -> Option<String> {
    Some(match value {
        Php::Null | Php::Bool(false) => String::new(),
        Php::Bool(true) => "1".to_owned(),
        Php::Int(number) => number.to_string(),
        Php::Float(number) => phpser::format_float(*number),
        Php::Str(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Php::Array(_) | Php::Object { .. } => return None,
    })
}

fn string_property(object: &Php, name: &str) -> String {
    property(object, name)
        .and_then(php_string)
        .unwrap_or_default()
}

fn key_string(key: &Key) -> String {
    match key {
        Key::Int(number) => number.to_string(),
        Key::Str(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn entries(value: &Php) -> &[(Key, Php)] {
    match value {
        Php::Array(entries) => entries,
        _ => &[],
    }
}

/// A result map, or the result older transcripts recorded in `legacy`
/// (`true`, `false` or `"forbidden"` with its reason in `reason`).
fn result_of(object: &Php, map: &str, legacy: &str, reason: &str, codes: [&str; 4]) -> ResultMap {
    if let Some(Php::Array(entries)) = property(object, map) {
        let get = |key: &str| {
            entries
                .iter()
                .find(|(entry, _)| *entry == Key::from(key))
                .map(|(_, value)| value)
        };
        return ResultMap {
            code: get("code").and_then(php_string).unwrap_or_default(),
            data: get("data")
                .cloned()
                .unwrap_or_else(|| Php::Array(Vec::new())),
        };
    }
    let [forbidden, matched, failed, unknown] = codes;
    match property(object, legacy) {
        Some(Php::Str(bytes)) if bytes == b"forbidden" => ResultMap {
            code: forbidden.to_owned(),
            data: Php::Array(vec![(
                Key::from("reason"),
                property(object, reason).cloned().unwrap_or(Php::Null),
            )]),
        },
        Some(Php::Bool(true)) => ResultMap {
            code: matched.to_owned(),
            data: Php::Array(Vec::new()),
        },
        Some(Php::Bool(false)) => ResultMap {
            code: failed.to_owned(),
            data: Php::Array(Vec::new()),
        },
        _ => ResultMap {
            code: unknown.to_owned(),
            data: Php::Array(Vec::new()),
        },
    }
}

fn object_transcript(value: &Php) -> Option<ObjectTranscript> {
    let Php::Object { class, .. } = value else {
        return None;
    };
    if class != "HeraldObjectTranscript" {
        return None;
    }
    let applied_transaction_phids = match property(value, "appliedTransactionPHIDs") {
        Some(Php::Array(entries)) => Some(
            entries
                .iter()
                .filter_map(|(_, phid)| php_string(phid))
                .collect(),
        ),
        _ => None,
    };
    let profile = match property(value, "profile") {
        Some(Php::Array(frames)) => Some(
            frames
                .iter()
                .map(|(_, frame)| {
                    let get = |key: &str| frame.get(&Key::from(key));
                    Frame {
                        kind: get("type").and_then(php_string).unwrap_or_default(),
                        key: get("key").and_then(php_string).unwrap_or_default(),
                        elapsed: match get("elapsed") {
                            Some(Php::Float(number)) => *number,
                            #[allow(clippy::cast_precision_loss)]
                            Some(Php::Int(number)) => *number as f64,
                            _ => 0.0,
                        },
                        count: match get("count") {
                            Some(Php::Int(number)) => *number,
                            _ => 0,
                        },
                    }
                })
                .collect(),
        ),
        _ => None,
    };
    Some(ObjectTranscript {
        phid: string_property(value, "phid"),
        content_type: string_property(value, "type"),
        name: string_property(value, "name"),
        fields: property(value, "fields")
            .map(entries)
            .unwrap_or_default()
            .iter()
            .map(|(key, value)| (key_string(key), value.clone()))
            .collect(),
        applied_transaction_phids,
        profile,
    })
}

/// The transcript with this ID.
pub async fn load_transcript(
    database: &Database,
    id: i64,
) -> Result<Option<Transcript>, TranscriptError> {
    let sql = format!(
        "SELECT id, phid, objectPHID, time, duration, dryRun, host, objectTranscript, \
         ruleTranscripts, conditionTranscripts, applyTranscripts FROM {} WHERE id = ?",
        database.table("herald", "herald_transcript"),
    );
    let Some(found) = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
    else {
        return Ok(None);
    };
    let row = TranscriptRow::from_row(&found)?;
    // `unserialize()` of a garbage collected column gives `false`.
    let column = |name: &str| -> Result<Php, TranscriptError> {
        let bytes: Option<Vec<u8>> =
            phab_db::sqlx::Row::try_get(&found, name).map_err(DbError::from)?;
        let bytes = bytes.unwrap_or_default();
        if bytes.is_empty() {
            return Ok(Php::Bool(false));
        }
        phpser::unserialize_objects(&bytes).map_err(|_| TranscriptError::Format(id))
    };
    let object = object_transcript(&column("objectTranscript")?);
    let rules = entries(&column("ruleTranscripts")?)
        .iter()
        .map(|(_, rule)| RuleTranscript {
            rule_id: string_property(rule, "ruleID"),
            name: string_property(rule, "ruleName"),
            owner: string_property(rule, "ruleOwner"),
            result: result_of(
                rule,
                "ruleResultMap",
                "result",
                "reason",
                ["object-state", "any-match", "any-failed", "unknown"],
            ),
        })
        .collect();
    let conditions = entries(&column("conditionTranscripts")?)
        .iter()
        .map(|(rule_id, conditions)| {
            let conditions = entries(conditions)
                .iter()
                .map(|(_, condition)| ConditionTranscript {
                    field: string_property(condition, "fieldName"),
                    condition: string_property(condition, "condition"),
                    test_value: property(condition, "testValue")
                        .cloned()
                        .unwrap_or(Php::Null),
                    result: result_of(
                        condition,
                        "resultMap",
                        "result",
                        "note",
                        ["object-state", "matched", "failed", "unknown"],
                    ),
                })
                .collect();
            (key_string(rule_id), conditions)
        })
        .collect();
    let applied = entries(&column("applyTranscripts")?)
        .iter()
        .map(|(_, apply)| ApplyTranscript {
            action: string_property(apply, "action"),
            target: property(apply, "target").cloned().unwrap_or(Php::Null),
            rule_id: string_property(apply, "ruleID"),
            applied_reason: property(apply, "appliedReason")
                .cloned()
                .unwrap_or(Php::Null),
        })
        .collect();
    Ok(Some(Transcript {
        row,
        object,
        rules,
        conditions,
        applied,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_results_are_mapped_like_php() {
        let legacy = |value: Php| Php::Object {
            class: "HeraldRuleTranscript".to_owned(),
            properties: vec![
                (phpser::protected("result"), value),
                (phpser::protected("reason"), Php::str("why")),
            ],
        };
        let codes = ["object-state", "any-match", "any-failed", "unknown"];
        let code = |value| result_of(&legacy(value), "ruleResultMap", "result", "reason", codes);
        assert_eq!(code(Php::Bool(true)).code, "any-match");
        assert_eq!(code(Php::Bool(false)).code, "any-failed");
        assert_eq!(code(Php::Null).code, "unknown");
        let forbidden = code(Php::str("forbidden"));
        assert_eq!(forbidden.code, "object-state");
        assert_eq!(
            forbidden.data.get(&Key::from("reason")),
            Some(&Php::str("why"))
        );
    }

    #[test]
    fn private_and_protected_properties_are_found() {
        let object = Php::Object {
            class: "HeraldApplyTranscript".to_owned(),
            properties: vec![
                (
                    phpser::private("HeraldApplyTranscript", "ruleID"),
                    Php::Int(3),
                ),
                (phpser::protected("name"), Php::str("x")),
            ],
        };
        assert_eq!(string_property(&object, "ruleID"), "3");
        assert_eq!(string_property(&object, "name"), "x");
        assert_eq!(string_property(&object, "missing"), "");
    }
}
