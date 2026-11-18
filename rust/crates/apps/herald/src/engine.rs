//! `HeraldEngine`: evaluating Herald rules against an object, the effects
//! of the rules which match, and the transcript of it all.
//!
//! Ported: global rules repeating on every edit, the fields and actions an
//! [`Adapter`] provides, and every condition but the regular expressions
//! and bit tests. Anything else is [`HeraldError::Unsupported`], found
//! before evaluation starts, so the caller can leave the edit to PHP.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use phab_db::phpser::{self, Key, Value as Php};
use phab_db::{Database, DbError, rows};
use thiserror::Error;

/// `HeraldRule::$configVersion`: rules written by newer versions are not
/// evaluated.
const CONFIG_VERSION: i64 = 38;

/// Errors of rule evaluation.
#[derive(Debug, Error)]
pub enum HeraldError {
    /// Something Rust doesn't implement; PHP should handle the edit.
    #[error("not ported: {0}")]
    Unsupported(&'static str),
    /// Loading rules or field values failed.
    #[error(transparent)]
    Database(#[from] DbError),
    /// A query failed.
    #[error(transparent)]
    Query(#[from] phab_db::sqlx::Error),
}

/// A `herald_condition` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// The row ID, as the string MySQL returns.
    pub id: String,
    /// The field, like `projects`.
    pub field: String,
    /// The condition, like `any`.
    pub condition: String,
    /// The value, decoded from JSON.
    pub value: Php,
}

/// A `herald_action` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRecord {
    /// The action, like `email.must-encrypt`.
    pub action: String,
    /// The target, decoded from JSON.
    pub target: Php,
}

/// A `herald_rule` row with its conditions and actions.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// The row ID, as the string MySQL returns.
    pub id: String,
    /// The rule PHID.
    pub phid: String,
    /// The name.
    pub name: String,
    /// Who wrote it.
    pub author_phid: String,
    /// `global`, `personal` or `object`.
    pub rule_type: String,
    /// Whether every condition must match.
    pub must_match_all: bool,
    /// The Herald version the rule was written with.
    pub config_version: i64,
    /// The conditions, in ID order.
    pub conditions: Vec<Condition>,
    /// The actions, in ID order.
    pub actions: Vec<ActionRecord>,
}

impl Rule {
    /// The ID as an integer.
    fn numeric_id(&self) -> i64 {
        self.id.parse().unwrap_or_default()
    }

    /// `getRuleExecutionOrderSortKey()`: personal, then object, then global
    /// rules, by ID.
    fn sort_key(&self) -> String {
        let order = match self.rule_type.as_str() {
            "personal" => 1,
            "object" => 2,
            _ => 3,
        };
        format!("~{order}{:010}", self.numeric_id())
    }
}

/// `HeraldEngine::loadRulesForAdapter()`: the enabled rules of a content
/// type, with their conditions and actions.
pub async fn load_rules(database: &Database, content_type: &str) -> Result<Vec<Rule>, HeraldError> {
    let sql = format!(
        "SELECT id, phid, name, authorPHID, ruleType, mustMatchAll, configVersion, \
         repetitionPolicy FROM {} WHERE isDisabled = 0 AND contentType = ? ORDER BY id",
        database.table("herald", "herald_rule")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(content_type)
        .fetch_all(database.pool())
        .await?;
    let mut rules = Vec::new();
    for row in &found {
        if rows::text(row, "ruleType")? != "global" {
            return Err(HeraldError::Unsupported("personal and object Herald rules"));
        }
        if rows::text(row, "repetitionPolicy")? != "every" {
            return Err(HeraldError::Unsupported(
                "Herald rules which remember where they applied",
            ));
        }
        rules.push(Rule {
            id: rows::int(row, "id")?.to_string(),
            phid: rows::text(row, "phid")?,
            name: rows::text(row, "name")?,
            author_phid: rows::text(row, "authorPHID")?,
            rule_type: rows::text(row, "ruleType")?,
            must_match_all: rows::flag(row, "mustMatchAll")?,
            config_version: rows::int(row, "configVersion")?,
            conditions: Vec::new(),
            actions: Vec::new(),
        });
    }
    if rules.is_empty() {
        return Ok(rules);
    }
    let ids: Vec<String> = rules.iter().map(|rule| rule.id.clone()).collect();
    let sql = format!(
        "SELECT id, ruleID, fieldName, fieldCondition, value FROM {} WHERE ruleID IN ({}) \
         ORDER BY id",
        database.table("herald", "herald_condition"),
        phab_db::placeholders(ids.len())
    );
    for row in phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?
    {
        let rule_id = rows::int(&row, "ruleID")?.to_string();
        let condition = Condition {
            id: rows::int(&row, "id")?.to_string(),
            field: rows::text(&row, "fieldName")?,
            condition: rows::text(&row, "fieldCondition")?,
            value: phpser::from_json(&rows::json(&row, "value")?),
        };
        if let Some(rule) = rules.iter_mut().find(|rule| rule.id == rule_id) {
            rule.conditions.push(condition);
        }
    }
    let sql = format!(
        "SELECT ruleID, action, target FROM {} WHERE ruleID IN ({}) ORDER BY id",
        database.table("herald", "herald_action"),
        phab_db::placeholders(ids.len())
    );
    for row in phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?
    {
        let rule_id = rows::int(&row, "ruleID")?.to_string();
        let action = ActionRecord {
            action: rows::text(&row, "action")?,
            target: phpser::from_json(&rows::json(&row, "target")?),
        };
        if let Some(rule) = rules.iter_mut().find(|rule| rule.id == rule_id) {
            rule.actions.push(action);
        }
    }
    Ok(rules)
}

/// What an adapter tells the engine about the object.
#[derive(Debug, Clone)]
pub struct Subject {
    /// The object's PHID.
    pub phid: String,
    /// `getAdapterContentType()`, like `differential`.
    pub content_type: String,
    /// `getHeraldName()`.
    pub name: String,
    /// The PHIDs of the transactions which just applied.
    pub applied_transaction_phids: Option<Vec<String>>,
}

/// The fields and actions an adapter implements.
pub trait Adapter {
    /// Whether the adapter can compute this field.
    fn has_field(&self, key: &str) -> bool;

    /// Whether the adapter can apply this action.
    fn has_action(&self, action: &str) -> bool;
}

/// `HeraldRuleResult` or `HeraldConditionResult`: a code and its data.
#[derive(Debug, Clone, PartialEq)]
struct ResultMap {
    code: &'static str,
    data: Php,
}

impl ResultMap {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            data: Php::Array(Vec::new()),
        }
    }

    fn with_exception(code: &'static str, class: &str, message: &str) -> Self {
        Self {
            code,
            data: Php::Array(vec![
                (Key::from("exception.class"), Php::str(class)),
                (Key::from("exception.message"), Php::str(message)),
            ]),
        }
    }

    /// `newResultMap()`.
    fn to_php(&self) -> Php {
        Php::Array(vec![
            (Key::from("code"), Php::str(self.code)),
            (Key::from("data"), self.data.clone()),
        ])
    }

    /// `getShouldApplyActions()` of a rule result.
    fn should_apply(&self) -> bool {
        matches!(self.code, "any-match" | "all-match")
    }
}

/// Why evaluation stopped, as PHP's exceptions.
#[derive(Debug, Clone)]
enum Interrupt {
    /// `HeraldInvalidConditionException`.
    Invalid(String),
    /// `HeraldRecursiveConditionsException`.
    Recursion,
    /// `HeraldRuleEvaluationException`.
    Evaluation,
}

/// An effect of a matching rule (`HeraldEffect`).
#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    /// The action.
    pub action: String,
    /// Its target.
    pub target: Php,
    /// The rule's ID.
    pub rule_id: String,
    /// The rule's PHID.
    pub rule_phid: String,
    /// "Conditions were met for H1 Name".
    pub reason: String,
}

/// A profiler frame of the object transcript.
#[derive(Debug, Clone)]
struct Frame {
    kind: &'static str,
    key: String,
    elapsed: f64,
    count: i64,
}

/// The transcript as it is being written (`HeraldTranscript`).
#[derive(Debug, Default)]
struct Transcripts {
    /// `ruleTranscripts`, by rule ID.
    rules: Vec<(String, Php)>,
    /// `conditionTranscripts`, by rule ID then condition ID.
    conditions: Vec<(String, Vec<(String, Php)>)>,
}

impl Transcripts {
    fn add_rule(&mut self, rule: &Rule, result: &ResultMap) {
        let transcript = Php::Object {
            class: "HeraldRuleTranscript".to_owned(),
            properties: vec![
                (phpser::protected("ruleID"), Php::str(&rule.id)),
                (phpser::protected("ruleResultMap"), result.to_php()),
                (phpser::protected("ruleName"), Php::str(&rule.name)),
                (phpser::protected("ruleOwner"), Php::str(&rule.author_phid)),
                (phpser::protected("reason"), Php::Null),
                (phpser::protected("result"), Php::Null),
            ],
        };
        set_entry(&mut self.rules, &rule.id, transcript);
    }

    fn add_condition(&mut self, rule: &Rule, condition: &Condition, result: &ResultMap) {
        let transcript = Php::Object {
            class: "HeraldConditionTranscript".to_owned(),
            properties: vec![
                (phpser::protected("ruleID"), Php::str(&rule.id)),
                (phpser::protected("conditionID"), Php::str(&condition.id)),
                (phpser::protected("fieldName"), Php::str(&condition.field)),
                (
                    phpser::protected("condition"),
                    Php::str(&condition.condition),
                ),
                (phpser::protected("testValue"), condition.value.clone()),
                (phpser::protected("resultMap"), result.to_php()),
                (phpser::protected("result"), Php::Null),
                (phpser::protected("note"), Php::Null),
            ],
        };
        let index = if let Some(index) = self.conditions.iter().position(|(id, _)| *id == rule.id) {
            index
        } else {
            self.conditions.push((rule.id.clone(), Vec::new()));
            self.conditions.len() - 1
        };
        set_entry(&mut self.conditions[index].1, &condition.id, transcript);
    }
}

/// `$array[$key] = $value`: replaces in place, or appends.
fn set_entry(entries: &mut Vec<(String, Php)>, key: &str, value: Php) {
    match entries.iter_mut().find(|(existing, _)| existing == key) {
        Some(entry) => entry.1 = value,
        None => entries.push((key.to_owned(), value)),
    }
}

/// An array keyed by the IDs in `entries`.
fn keyed(entries: Vec<(String, Php)>) -> Php {
    Php::Array(
        entries
            .into_iter()
            .map(|(key, value)| (Key::from(key.as_str()), value))
            .collect(),
    )
}

/// The outcome of [`evaluate()`]: the effects to apply and the transcript
/// so far.
#[derive(Debug)]
pub struct Evaluation {
    /// The effects of the rules which matched, in order.
    pub effects: Vec<Effect>,
    object_transcript: Php,
    rule_transcripts: Php,
    condition_transcripts: Php,
    duration: f64,
}

/// The engine's state during [`evaluate()`].
struct Engine<'a> {
    rules: Vec<&'a Rule>,
    values: &'a [(String, Php)],
    field_cache: Vec<(String, Php)>,
    frames: Vec<Frame>,
    results: HashMap<String, ResultMap>,
    rule_stack: Vec<String>,
    transcripts: Transcripts,
}

impl<'a> Engine<'a> {
    fn rule(&self, phid: &str) -> Option<&'a Rule> {
        self.rules.iter().copied().find(|rule| rule.phid == phid)
    }

    fn set_rule_result(&mut self, rule: &Rule, result: ResultMap) {
        self.transcripts.add_rule(rule, &result);
        self.results.insert(rule.phid.clone(), result);
    }

    /// `popProfilerStack()`: accumulates a frame.
    fn record_frame(&mut self, kind: &'static str, key: &str, started: Instant) {
        let elapsed = started.elapsed().as_secs_f64();
        match self
            .frames
            .iter_mut()
            .find(|frame| frame.kind == kind && frame.key == key)
        {
            Some(frame) => {
                frame.elapsed += elapsed;
                frame.count += 1;
            }
            None => self.frames.push(Frame {
                kind,
                key: key.to_owned(),
                elapsed,
                count: 1,
            }),
        }
    }

    /// `getProfiledObjectFieldValue()`.
    fn field(&mut self, key: &str) -> Php {
        let started = Instant::now();
        let value = if let Some((_, value)) = self.field_cache.iter().find(|(name, _)| name == key)
        {
            value.clone()
        } else {
            let value = self
                .values
                .iter()
                .find(|(name, _)| name == key)
                .map_or(Php::Null, |(_, value)| value.clone());
            self.field_cache.push((key.to_owned(), value.clone()));
            value
        };
        self.record_frame("field", key, started);
        value
    }

    /// `getRuleMatchResult()`.
    fn rule_match(&mut self, rule: &'a Rule) -> Result<ResultMap, Interrupt> {
        if let Some(result) = self.results.get(&rule.phid) {
            return Ok(result.clone());
        }
        if self.rule_stack.contains(&rule.phid) {
            return Err(Interrupt::Recursion);
        }
        self.rule_stack.push(rule.phid.clone());
        let code = if rule.config_version > CONFIG_VERSION {
            "version"
        } else if rule.conditions.is_empty() {
            "empty"
        } else {
            let mut code = None;
            for condition in &rule.conditions {
                let matched = match self.condition_match(rule, condition) {
                    Ok(matched) => matched,
                    Err(Interrupt::Recursion) => return Err(Interrupt::Recursion),
                    Err(_) => return Err(Interrupt::Evaluation),
                };
                if !rule.must_match_all && matched {
                    code = Some("any-match");
                    break;
                }
                if rule.must_match_all && !matched {
                    code = Some("any-failed");
                    break;
                }
            }
            code.unwrap_or(if rule.must_match_all {
                "all-match"
            } else {
                "all-failed"
            })
        };
        let result = ResultMap::new(code);
        self.set_rule_result(rule, result.clone());
        Ok(result)
    }

    /// `doesConditionMatch()` of the engine: the condition's transcript,
    /// made before the test (so it comes before those of the rules the
    /// test evaluates) and completed after it.
    fn condition_match(
        &mut self,
        rule: &'a Rule,
        condition: &Condition,
    ) -> Result<bool, Interrupt> {
        self.transcripts
            .add_condition(rule, condition, &ResultMap::new("unknown"));
        let value = self.field(&condition.field);
        let started = Instant::now();
        let outcome = self.test(condition, &value);
        self.record_frame("rule", &rule.phid, started);
        let result = match &outcome {
            Ok(true) => ResultMap::new("matched"),
            Ok(false) => ResultMap::new("failed"),
            Err(Interrupt::Recursion) => {
                ResultMap::with_exception("recursion", "HeraldRecursiveConditionsException", "")
            }
            Err(Interrupt::Invalid(message)) => {
                ResultMap::with_exception("invalid", "HeraldInvalidConditionException", message)
            }
            Err(Interrupt::Evaluation) => {
                ResultMap::with_exception("exception", "HeraldRuleEvaluationException", "")
            }
        };
        self.transcripts.add_condition(rule, condition, &result);
        outcome
    }

    /// `HeraldAdapter::doesConditionMatch()`.
    fn test(&mut self, condition: &Condition, field: &Php) -> Result<bool, Interrupt> {
        let expected = &condition.value;
        let invalid = |message: &str| Interrupt::Invalid(message.to_owned());
        Ok(match condition.condition.as_str() {
            "contains" | "!contains" => {
                let wanted = condition.condition == "contains";
                let needle = php_string(expected).to_ascii_lowercase();
                let found = as_list(field)
                    .iter()
                    .any(|value| contains(&php_string(value).to_ascii_lowercase(), &needle));
                if found { wanted } else { !wanted }
            }
            "isany" | "!isany" => {
                let Php::Array(values) = expected else {
                    return Err(invalid("Expected condition value to be an array."));
                };
                let found = !matches!(field, Php::Null)
                    && values
                        .iter()
                        .any(|(_, value)| fuse_key(value) == fuse_key(field));
                if condition.condition == "isany" {
                    found
                } else {
                    !found
                }
            }
            "all" => {
                let Php::Array(_) = field else {
                    return Err(invalid("Object produced non-array value!"));
                };
                let Php::Array(wanted) = expected else {
                    return Err(invalid("Expected condition value to be an array."));
                };
                let have = fused_keys(field);
                let present: HashSet<Key> = wanted
                    .iter()
                    .filter_map(|(_, value)| fuse_key(value))
                    .filter(|key| have.contains(key))
                    .collect();
                present.len() == wanted.len()
            }
            "any" | "none" => {
                let have = fused_keys(field);
                let found = as_list(expected)
                    .iter()
                    .filter_map(fuse_key)
                    .any(|key| have.contains(&key));
                if condition.condition == "any" {
                    found
                } else {
                    !found
                }
            }
            "exists" | "true" | "unconditionally" => phab_core_truthy(field),
            "!exists" | "false" => !phab_core_truthy(field),
            "never" => false,
            "conditions" | "!conditions" => {
                let Some(phid) = expected
                    .as_bytes()
                    .and_then(|b| std::str::from_utf8(b).ok())
                else {
                    return Err(invalid("Condition references a rule which does not exist!"));
                };
                let Some(other) = self.rule(phid) else {
                    return Err(invalid("Condition references a rule which does not exist!"));
                };
                let matched = self.rule_match(other)?.should_apply();
                if condition.condition == "conditions" {
                    matched
                } else {
                    !matched
                }
            }
            _ => return Err(invalid("not ported")),
        })
    }
}

/// `(string)` of a scalar.
fn php_string(value: &Php) -> String {
    match value {
        Php::Str(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Php::Int(number) => number.to_string(),
        Php::Bool(true) => "1".to_owned(),
        Php::Float(number) => phpser::format_float(*number),
        _ => String::new(),
    }
}

fn contains(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

/// `(array)` of a value.
fn as_list(value: &Php) -> Vec<Php> {
    match value {
        Php::Null => Vec::new(),
        Php::Array(entries) => entries.iter().map(|(_, value)| value.clone()).collect(),
        other => vec![other.clone()],
    }
}

/// The key `array_fuse()` makes of a value.
fn fuse_key(value: &Php) -> Option<Key> {
    match value {
        Php::Str(bytes) => Some(Key::from_bytes(bytes.clone())),
        Php::Int(number) => Some(Key::Int(*number)),
        Php::Bool(flag) => Some(Key::Int(i64::from(*flag))),
        Php::Null => Some(Key::Str(Vec::new())),
        _ => None,
    }
}

fn fused_keys(value: &Php) -> HashSet<Key> {
    as_list(value).iter().filter_map(fuse_key).collect()
}

/// PHP's truthiness of a value.
fn phab_core_truthy(value: &Php) -> bool {
    match value {
        Php::Null => false,
        Php::Bool(flag) => *flag,
        Php::Int(number) => *number != 0,
        Php::Float(number) => *number != 0.0,
        Php::Str(bytes) => !(bytes.is_empty() || bytes == b"0"),
        Php::Array(entries) => !entries.is_empty(),
        Php::Object { .. } => true,
    }
}

/// The conditions [`evaluate()`] implements.
const CONDITIONS: &[&str] = &[
    "contains",
    "!contains",
    "isany",
    "!isany",
    "all",
    "any",
    "none",
    "exists",
    "!exists",
    "true",
    "false",
    "unconditionally",
    "never",
    "conditions",
    "!conditions",
];

/// Whether `adapter` can evaluate `rules` and apply their actions.
pub fn check_supported(rules: &[Rule], adapter: &impl Adapter) -> Result<(), HeraldError> {
    for rule in rules {
        for condition in &rule.conditions {
            if !adapter.has_field(&condition.field) {
                return Err(HeraldError::Unsupported("a Herald field not ported"));
            }
            if !CONDITIONS.contains(&condition.condition.as_str()) {
                return Err(HeraldError::Unsupported("a Herald condition not ported"));
            }
        }
        for action in &rule.actions {
            if !adapter.has_action(&action.action) {
                return Err(HeraldError::Unsupported("a Herald action not ported"));
            }
        }
    }
    Ok(())
}

/// The fields the conditions of `rules` read.
pub fn fields_used(rules: &[Rule]) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    for condition in rules.iter().flat_map(|rule| &rule.conditions) {
        if !fields.contains(&condition.field) {
            fields.push(condition.field.clone());
        }
    }
    fields
}

/// `HeraldEngine::applyRules()`: evaluates `rules` against the field
/// `values` the adapter computed, and collects the effects.
pub fn evaluate(rules: &[Rule], subject: &Subject, values: &[(String, Php)]) -> Evaluation {
    let started = Instant::now();
    let mut ordered: Vec<&Rule> = rules.iter().collect();
    ordered.sort_by_key(|rule| rule.sort_key());
    let mut engine = Engine {
        rules: ordered.clone(),
        values,
        field_cache: Vec::new(),
        frames: Vec::new(),
        results: HashMap::new(),
        rule_stack: Vec::new(),
        transcripts: Transcripts::default(),
    };
    let mut effects = Vec::new();
    for rule in ordered {
        engine.rule_stack.clear();
        let result = match engine.rule_match(rule) {
            Ok(result) => Some(result),
            Err(Interrupt::Recursion) => {
                engine.fail_cycle(rule);
                None
            }
            Err(_) => Some(ResultMap::new("evaluation-exception")),
        };
        if !engine.results.contains_key(&rule.phid)
            && let Some(result) = result
        {
            engine.set_rule_result(rule, result);
        }
        if engine
            .results
            .get(&rule.phid)
            .is_some_and(ResultMap::should_apply)
        {
            for action in &rule.actions {
                effects.push(Effect {
                    action: action.action.clone(),
                    target: action.target.clone(),
                    rule_id: rule.id.clone(),
                    rule_phid: rule.phid.clone(),
                    reason: format!("Conditions were met for H{} {}", rule.id, rule.name),
                });
            }
        }
    }
    let object_transcript = engine.object_transcript(subject);
    let conditions = engine
        .transcripts
        .conditions
        .into_iter()
        .map(|(rule, entries)| (rule, keyed(entries)))
        .collect();
    Evaluation {
        effects,
        object_transcript,
        rule_transcripts: keyed(engine.transcripts.rules),
        condition_transcripts: keyed(conditions),
        duration: started.elapsed().as_secs_f64(),
    }
}

impl Engine<'_> {
    /// A dependency cycle: the rules on the stack which have no result
    /// fail with the cycle.
    fn fail_cycle(&mut self, rule: &Rule) {
        let mut cycle: Vec<Php> = self.rule_stack.iter().map(|phid| Php::str(phid)).collect();
        cycle.push(Php::str(&rule.phid));
        for phid in self.rule_stack.clone() {
            if self.results.contains_key(&phid) {
                continue;
            }
            if let Some(stacked) = self.rule(&phid) {
                let result = ResultMap {
                    code: "recursion",
                    data: Php::Array(vec![(Key::from("cyclePHIDs"), Php::list(cycle.clone()))]),
                };
                self.set_rule_result(stacked, result);
            }
        }
    }

    /// `HeraldObjectTranscript`: the fields read, the transactions and the
    /// profile.
    fn object_transcript(&self, subject: &Subject) -> Php {
        let fields = self
            .field_cache
            .iter()
            .map(|(key, value)| (Key::from(key.as_str()), truncate(value.clone(), 4096).0))
            .collect();
        let profile = Php::list(self.frames.iter().map(|frame| {
            Php::Array(vec![
                (Key::from("type"), Php::str(frame.kind)),
                (Key::from("key"), Php::str(&frame.key)),
                (Key::from("elapsed"), Php::Float(frame.elapsed)),
                (Key::from("count"), Php::Int(frame.count)),
            ])
        }));
        let applied = subject
            .applied_transaction_phids
            .as_ref()
            .map_or(Php::Null, |phids| {
                Php::list(phids.iter().map(|phid| Php::str(phid)))
            });
        Php::Object {
            class: "HeraldObjectTranscript".to_owned(),
            properties: vec![
                (phpser::protected("phid"), Php::str(&subject.phid)),
                (phpser::protected("type"), Php::str(&subject.content_type)),
                (phpser::protected("name"), Php::str(&subject.name)),
                (phpser::protected("fields"), Php::Array(fields)),
                (phpser::protected("appliedTransactionPHIDs"), applied),
                (phpser::protected("profile"), profile),
            ],
        }
    }
}

/// `HeraldObjectTranscript::truncateValue()`: the value and its length.
fn truncate(value: Php, length: i64) -> (Php, i64) {
    match value {
        Php::Str(bytes) => {
            let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
            if size <= length {
                (Php::Str(bytes), size)
            } else {
                let cut = usize::try_from(length.max(0)).unwrap_or_default();
                let mut text = String::from_utf8_lossy(&bytes[..cut]).into_owned();
                text.push_str("\n<...>");
                let size = i64::try_from(text.len()).unwrap_or(i64::MAX);
                (Php::str(&text), size)
            }
        }
        Php::Array(entries) => {
            let mut remaining = length;
            let mut output = Vec::new();
            let mut cut = false;
            for (key, entry) in entries {
                if remaining <= 0 {
                    cut = true;
                    continue;
                }
                let (entry, size) = truncate(entry, remaining);
                remaining -= size;
                output.push((key, entry));
            }
            if cut {
                output.push((Key::from("<...>"), Php::str("<...>")));
            }
            (Php::Array(output), 0)
        }
        other => (other, 0),
    }
}

/// `HeraldApplyTranscript`.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The effect.
    pub effect: Effect,
    /// Whether it applied.
    pub applied: bool,
    /// `appliedReason`: what `logEffect()` recorded.
    pub reasons: Vec<(String, Php)>,
}

impl Applied {
    fn to_php(&self) -> Php {
        let private = |name| phpser::private("HeraldApplyTranscript", name);
        let reasons = Php::list(self.reasons.iter().map(|(kind, data)| {
            Php::Array(vec![
                (Key::from("type"), Php::str(kind)),
                (Key::from("data"), data.clone()),
            ])
        }));
        Php::Object {
            class: "HeraldApplyTranscript".to_owned(),
            properties: vec![
                (private("action"), Php::str(&self.effect.action)),
                (private("target"), self.effect.target.clone()),
                (private("ruleID"), Php::str(&self.effect.rule_id)),
                (private("reason"), Php::str(&self.effect.reason)),
                (private("applied"), Php::Bool(self.applied)),
                (private("appliedReason"), reasons),
            ],
        }
    }
}

/// A `herald_transcript` row, ready to insert.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRow {
    /// The object.
    pub object_phid: String,
    /// How long evaluation took, in seconds.
    pub duration: f64,
    /// The serialized `objectTranscript`.
    pub object_transcript: Vec<u8>,
    /// The serialized `ruleTranscripts`.
    pub rule_transcripts: Vec<u8>,
    /// The serialized `conditionTranscripts`.
    pub condition_transcripts: Vec<u8>,
    /// The serialized `applyTranscripts`.
    pub apply_transcripts: Vec<u8>,
}

impl Evaluation {
    /// The transcript row, once the effects applied.
    pub fn transcript(&self, subject: &Subject, applied: &[Applied]) -> TranscriptRow {
        TranscriptRow {
            object_phid: subject.phid.clone(),
            duration: self.duration,
            object_transcript: phpser::serialize(&self.object_transcript),
            rule_transcripts: phpser::serialize(&self.rule_transcripts),
            condition_transcripts: phpser::serialize(&self.condition_transcripts),
            apply_transcripts: phpser::serialize(&Php::list(applied.iter().map(Applied::to_php))),
        }
    }
}

/// `getXHeraldRulesHeader()`: the IDs of the rules whose effects applied.
pub fn rules_header(applied: &[Applied]) -> String {
    let mut ids: Vec<&str> = Vec::new();
    for transcript in applied.iter().filter(|transcript| transcript.applied) {
        if !ids.contains(&transcript.effect.rule_id.as_str()) {
            ids.push(&transcript.effect.rule_id);
        }
    }
    if ids.is_empty() {
        return "none".to_owned();
    }
    ids.iter()
        .map(|id| format!("<{id}>"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `combineXHeraldRulesHeaders()`: the saved header and the new one.
pub fn combine_headers(saved: Option<&str>, new: &str) -> String {
    let Some(saved) = saved else {
        return new.to_owned();
    };
    let mut combined: Vec<&str> = Vec::new();
    for part in saved
        .split([',', ' '])
        .chain(new.split([',', ' ']))
        .filter(|part| !part.is_empty() && *part != "0")
    {
        if !combined.contains(&part) {
            combined.push(part);
        }
    }
    combined.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, all: bool, conditions: Vec<Condition>, actions: Vec<ActionRecord>) -> Rule {
        Rule {
            id: id.to_owned(),
            phid: format!("PHID-HRUL-{id}"),
            name: format!("Rule {id}"),
            author_phid: "PHID-USER-owner".to_owned(),
            rule_type: "global".to_owned(),
            must_match_all: all,
            config_version: 38,
            conditions,
            actions,
        }
    }

    fn condition(id: &str, field: &str, test: &str, value: Php) -> Condition {
        Condition {
            id: id.to_owned(),
            field: field.to_owned(),
            condition: test.to_owned(),
            value,
        }
    }

    /// Mozilla's secure revision rules, on an edit of a tagged revision.
    #[test]
    fn evaluates_like_php() {
        let rules = vec![
            rule(
                "1",
                true,
                vec![condition("1", "new-object", "true", Php::Null)],
                vec![ActionRecord {
                    action: "projects.add".to_owned(),
                    target: Php::list([Php::str("PHID-PROJ-secure")]),
                }],
            ),
            rule(
                "2",
                false,
                vec![
                    condition("2", "rule", "conditions", Php::str("PHID-HRUL-1")),
                    condition(
                        "3",
                        "projects",
                        "any",
                        Php::list([Php::str("PHID-PROJ-secure")]),
                    ),
                ],
                vec![ActionRecord {
                    action: "email.must-encrypt".to_owned(),
                    target: Php::Null,
                }],
            ),
        ];
        let subject = Subject {
            phid: "PHID-DREV-1".to_owned(),
            content_type: "differential".to_owned(),
            name: "Title".to_owned(),
            applied_transaction_phids: Some(vec!["PHID-XACT-DREV-1".to_owned()]),
        };
        let values: Vec<(String, Php)> = vec![
            ("new-object".to_owned(), Php::Bool(false)),
            ("rule".to_owned(), Php::Null),
            (
                "projects".to_owned(),
                Php::list([Php::str("PHID-PROJ-other"), Php::str("PHID-PROJ-secure")]),
            ),
        ];
        let evaluation = evaluate(&rules, &subject, &values);
        assert_eq!(evaluation.effects.len(), 1);
        assert_eq!(
            evaluation.effects[0].reason,
            "Conditions were met for H2 Rule 2"
        );
        let rules = String::from_utf8(phpser::serialize(&evaluation.rule_transcripts)).unwrap();
        assert!(
            rules.contains("s:4:\"code\";s:10:\"any-failed\""),
            "{rules}"
        );
        assert!(rules.contains("s:4:\"code\";s:9:\"any-match\""), "{rules}");
        // The profile: field, rule 1, field, rule 2 (twice), field.
        let Php::Object { properties, .. } = &evaluation.object_transcript else {
            panic!("an object");
        };
        let Php::Array(profile) = &properties[5].1 else {
            panic!("a profile");
        };
        let frames: Vec<(String, i64)> = profile
            .iter()
            .map(|(_, frame)| {
                let key = php_string(frame.get(&Key::from("key")).unwrap());
                let Some(Php::Int(count)) = frame.get(&Key::from("count")) else {
                    panic!("a count");
                };
                (key, *count)
            })
            .collect();
        assert_eq!(
            frames,
            [
                ("new-object".to_owned(), 1),
                ("PHID-HRUL-1".to_owned(), 1),
                ("rule".to_owned(), 1),
                ("PHID-HRUL-2".to_owned(), 2),
                ("projects".to_owned(), 1),
            ]
        );
    }

    #[test]
    fn combines_headers_like_php() {
        assert_eq!(combine_headers(None, "<2>"), "<2>");
        assert_eq!(combine_headers(Some("<1>, <2>"), "<2>"), "<1>, <2>");
        assert_eq!(combine_headers(Some("<1>"), "none"), "<1>, none");
    }
}
