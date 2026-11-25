//! `/herald/edit/[{id}/]` (`HeraldRuleController`): the rule editor, for
//! global and personal revision rules (`differential`) whose fields and
//! actions the Rust Herald engine implements.
//!
//! PHP's form is built by the `herald-rule-editor` script, which posts the
//! conditions and actions as JSON (`rule`: `{"conditions": [[field,
//! condition, value], ...], "actions": [[action, target], ...]}`, a
//! tokenizer's value being a map keyed by PHID). The Rust pages don't use
//! PHP's scripts: the Rust form is plain HTML posting the same fields
//! (`name`, `must_match`, `repetition_policy`, `rule`, `content_type`,
//! `rule_type`, `save`), with the JSON in a text area. The post is
//! `saveRule()`'s: the same validation errors, then the Rust rule editor
//! ([`phab_app_herald::editor`]).
//!
//! Left to PHP (`421`): object rules, other adapters, rules whose fields,
//! conditions or actions aren't ported (the engine would leave revision
//! edits to PHP), rules of a newer Herald, bit conditions, regular
//! expressions Rust can't decide on, posts PHP answers with an exception
//! (rule data which doesn't decode, without conditions or actions, unknown
//! actions, targets which aren't lists), and what the editor leaves to PHP
//! (saves without effect, rules about Herald rules).

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_herald::editor::{ActionValue, Change, Components, ConditionValue, Target};
use phab_app_herald::web::{self as herald, RuleFilter};
use phab_core::Phid;
use serde_json::{Map, Value, json};

use super::create::{ADAPTERS, can_manage_global};
use super::edit::{Refusal, apply_validated};
use super::rule::{can_edit, visible_rules};
use super::{APPLICATION, Kind, REVISIONS, field, load_handles, status_name};
use crate::error::PageError;
use crate::harbormaster::{self, Handle, Scope};
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{project_edit, web_edit};

/// The fields of the revision adapter the Rust engine computes.
const FIELDS: &[&str] = &[
    "differential.revision.title",
    "differential.revision.summary",
    "differential.revision.test-plan",
    "differential.revision.author",
    "differential.revision.author.projects",
    "differential.revision.reviewers",
    "differential.revision.repository",
    "differential.revision.repository.projects",
    "revision.status",
    "differential.revision.diff.affected",
    "differential.revision.diff.content",
    "differential.revision.diff.new",
    "differential.revision.diff.old",
    "cc",
    "projects",
    "herald.acting-user",
    "contentsource",
    "always",
    "new-object",
];

/// The actions of the revision adapter the Rust engine applies, and
/// whether their targets are PHID lists (`STANDARD_PHID_LIST`, whose
/// `willSaveActionValue()` keeps the keys of the tokenizer's map).
const ACTIONS: &[(&str, bool)] = &[
    ("subscribers.add", true),
    ("subscribers.remove", true),
    ("subscribers.self.add", false),
    ("subscribers.self.remove", false),
    ("projects.add", true),
    ("projects.remove", true),
    ("email.other", true),
    ("email.self", false),
    ("email.must-encrypt", false),
    ("differential.reviewers.add", true),
    ("differential.reviewers.blocking", true),
    ("differential.reviewers.self.add", false),
    ("differential.reviewers.self.blocking", false),
    ("nothing", false),
];

/// The conditions `willSaveCondition()` knows, but the bit tests.
const CONDITIONS: &[&str] = &[
    "contains",
    "!contains",
    "is",
    "!is",
    "isany",
    "!isany",
    "all",
    "any",
    "none",
    "me",
    "!me",
    "conditions",
    "!conditions",
    "exists",
    "!exists",
    "unconditionally",
    "never",
    "true",
    "false",
    "regexp",
    "!regexp",
    "regexp-pair",
];

/// `getRepetitionOptionMap()` of the revision adapter.
const REPETITION: &[(&str, &str)] = &[
    ("every", "every time this rule matches:"),
    ("first", "only the first time this rule matches:"),
    ("change", "if this rule did not match the last time:"),
];

#[derive(Template)]
#[template(path = "herald_rule_edit.html")]
pub(crate) struct EditPage {
    shell: Shell,
    title: String,
    action: String,
    errors: Vec<String>,
    name_error: Option<&'static str>,
    name: String,
    content_type: String,
    rule_type: String,
    /// "This Global rule triggers for Differential Revisions."
    rule_type_name: &'static str,
    content_type_name: &'static str,
    match_all: bool,
    /// The options and whether each is selected.
    repetition: Vec<(&'static str, &'static str, bool)>,
    rule_json: String,
    fields: &'static [&'static str],
    actions: Vec<&'static str>,
    cancel: String,
}

/// The rule the form edits.
struct Edited {
    target: Target,
    id: Option<i64>,
    name: String,
    content_type: String,
    rule_type: String,
    match_all: bool,
    repetition_policy: String,
    rule_json: String,
}

/// `getEditorValue()` of a stored value: tokenizers get a map of the
/// values to their names, which saving turns back into the list.
fn editor_value(value: &Value, tokens: bool, handles: &HashMap<Phid, Handle>) -> Value {
    if !tokens {
        return value.clone();
    }
    let mut map = Map::new();
    if let Some(entries) = entries(value) {
        for entry in entries {
            let key = phab_core::php::string_cast(entry).into_owned();
            let name = if key.starts_with("PHID-") {
                harbormaster::handle(handles, &Phid::new(key.as_str()))
                    .full_name
                    .clone()
            } else {
                status_name(&key).unwrap_or(&key).to_owned()
            };
            map.insert(key, json!(name));
        }
    }
    Value::Object(map)
}

/// Whether a field's values are a tokenizer's.
fn is_token_field(key: &str, condition: &str) -> bool {
    matches!(
        field(key),
        Some((_, Kind::Phid | Kind::PhidList | Kind::Status))
    ) && !matches!(condition, "exists" | "!exists")
}

/// The existing rule, as its form shows it.
async fn existing<S: WebState>(state: &S, viewer: &WebUser, id: i64) -> Result<Edited, PageError> {
    let rule = herald::load_rules(
        state.database(),
        &RuleFilter {
            ids: Some(&[id]),
            ..RuleFilter::default()
        },
    )
    .await?
    .pop()
    .ok_or(PageError::NotFound)?;
    if rule.is_object_rule() {
        return Err(PageError::Fallback("an object Herald rule"));
    }
    let scope = Scope::new(state, viewer);
    let rule = visible_rules(state, &scope, vec![rule])
        .await?
        .pop()
        .ok_or(PageError::PolicyDenied)?;
    if !can_edit(state, &scope, &rule).await? {
        return Err(PageError::PolicyDenied);
    }
    if rule.content_type != REVISIONS {
        return Err(PageError::Fallback(
            "a Herald rule of an adapter not ported",
        ));
    }
    if phab_app_herald::editor::config_version(state.database(), id)
        .await?
        .is_some_and(|version| version > phab_app_herald::editor::CONFIG_VERSION)
    {
        return Err(PageError::Fallback("Herald rules of a newer version"));
    }
    let (conditions, actions) = phab_app_herald::editor::load_components(state.database(), id)
        .await
        .map_err(|_| PageError::Fallback("rules whose conditions don't load"))?;
    let phids_in = |value: &Value| -> Vec<Phid> {
        entries(value)
            .unwrap_or_default()
            .into_iter()
            .filter_map(Value::as_str)
            .filter(|text| text.starts_with("PHID-"))
            .map(Phid::new)
            .collect()
    };
    let mut phids: Vec<Phid> = Vec::new();
    for condition in &conditions {
        if !FIELDS.contains(&condition.field.as_str())
            || !CONDITIONS.contains(&condition.condition.as_str())
        {
            return Err(PageError::Fallback("a Herald condition not ported"));
        }
        if is_token_field(&condition.field, &condition.condition) {
            phids.extend(phids_in(&condition.value));
        }
    }
    for action in &actions {
        let Some((_, list)) = ACTIONS.iter().find(|(key, _)| *key == action.action) else {
            return Err(PageError::Fallback("a Herald action not ported"));
        };
        if *list {
            phids.extend(phids_in(&action.target));
        }
    }
    let handles = load_handles(state, &scope, &phids).await?;
    Ok(Edited {
        target: Target::Rule(id),
        id: Some(id),
        name: rule.name,
        content_type: rule.content_type,
        rule_type: rule.rule_type,
        match_all: rule.must_match_all,
        repetition_policy: rule.repetition_policy,
        rule_json: editor_json(&conditions, &actions, &handles),
    })
}

/// The `rule` the form starts with, as `setupEditorBehavior()` gives the
/// script the conditions and actions.
fn editor_json(
    conditions: &[ConditionValue],
    actions: &[ActionValue],
    handles: &HashMap<Phid, Handle>,
) -> String {
    let conditions: Vec<Value> = conditions
        .iter()
        .map(|condition| {
            let tokens = is_token_field(&condition.field, &condition.condition);
            json!([
                condition.field,
                condition.condition,
                editor_value(&condition.value, tokens, handles)
            ])
        })
        .collect();
    let actions: Vec<Value> = actions
        .iter()
        .map(|action| {
            let tokens = ACTIONS
                .iter()
                .any(|(key, list)| *key == action.action && *list);
            json!([action.action, editor_value(&action.target, tokens, handles)])
        })
        .collect();
    serde_json::to_string_pretty(&json!({
        "conditions": conditions,
        "actions": actions,
    }))
    .unwrap_or_default()
}

/// A dialog explaining why the rule can't be created.
fn refusal<S: WebState>(
    state: &S,
    viewer: &WebUser,
    uri: &Uri,
    title: &'static str,
    paragraph: String,
) -> Response {
    project_edit::dialog_page(
        state,
        viewer,
        uri,
        (title, vec![paragraph], None),
        Vec::new(),
        "/herald/new/".to_owned(),
    )
    .into_response()
}

/// `array_keys()` of a decoded JSON array or object: indexes, and keys
/// which are canonical integers, are integers.
fn array_keys(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(items) => Some((0..items.len()).map(|index| json!(index)).collect()),
        Value::Object(map) => Some(
            map.keys()
                .map(|key| match key.parse::<i64>() {
                    Ok(number) if number.to_string() == *key => json!(number),
                    _ => json!(key),
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The entries of a decoded JSON list or object, in order.
fn entries(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Object(map) => Some(map.values().collect()),
        _ => None,
    }
}

/// `@preg_match($pattern, '')` fails: `Some(true)` when PHP fails,
/// `Some(false)` when it compiles, `None` when Rust can't tell.
fn regexp_fails(pattern: &str) -> Option<bool> {
    if phab_remarkup::preg::compile_php(pattern.as_bytes()).is_some() {
        return Some(false);
    }
    let first = pattern.chars().next();
    match first {
        None => Some(true),
        Some(first) if first.is_ascii_alphanumeric() || first == '\\' => Some(true),
        _ => None,
    }
}

/// `willSaveCondition()`: its error, if any.
fn condition_error(condition: &str, value: &Value) -> Result<Option<String>, PageError> {
    let undecided = || PageError::Fallback("regular expressions Rust can't validate");
    match condition {
        "regexp" | "!regexp" => {
            let Value::String(pattern) = value else {
                return Err(undecided());
            };
            Ok(regexp_fails(pattern).ok_or_else(undecided)?.then(|| {
                format!(
                    "The regular expression \"{pattern}\" is not valid. Regular expressions must \
                     have enclosing characters (e.g. \"@/path/to/file@\", not \"/path/to/file\") \
                     and be syntactically correct."
                )
            }))
        }
        "regexp-pair" => {
            let Value::String(text) = value else {
                return Err(undecided());
            };
            let Some(pair) = serde_json::from_str::<Value>(text)
                .ok()
                .filter(|json| json.is_array() || json.is_object())
            else {
                return Ok(Some(format!(
                    "The regular expression pair \"{text}\" is not valid JSON. Enter a valid JSON \
                     array with two elements."
                )));
            };
            let items = entries(&pair).unwrap_or_default();
            if items.len() != 2 {
                return Ok(Some(format!(
                    "The regular expression pair \"{text}\" must have exactly two elements."
                )));
            }
            let (Value::String(key), Value::String(val)) = (items[0], items[1]) else {
                return Err(undecided());
            };
            if regexp_fails(key).ok_or_else(undecided)? {
                return Ok(Some(format!(
                    "The first regexp in the regexp pair, \"{key}\", is not a valid regexp."
                )));
            }
            if regexp_fails(val).ok_or_else(undecided)? {
                return Ok(Some(format!(
                    "The second regexp in the regexp pair, \"{val}\", is not a valid regexp."
                )));
            }
            Ok(None)
        }
        "bit" | "!bit" => Err(PageError::Fallback("bit conditions")),
        known if CONDITIONS.contains(&known) => Ok(None),
        other => Ok(Some(format!("Unknown condition \"{other}\"!"))),
    }
}

/// The conditions and actions of a post, and its errors.
type PostedRule = (Vec<ConditionValue>, Vec<ActionValue>, Vec<String>);

/// `saveRule()`'s reading of the posted `rule`: the conditions and
/// actions, and the errors of `willSaveCondition()`.
fn posted_rule(json: Option<&str>) -> Result<PostedRule, PageError> {
    let exception = || PageError::Fallback("rule data PHP fails to decode");
    let data: Value = serde_json::from_str(json.ok_or_else(exception)?).map_err(|_| exception())?;
    let Value::Object(data) = data else {
        return Err(exception());
    };
    let truthy = |key: &str| data.get(key).is_some_and(phab_core::php::is_truthy);
    if !truthy("conditions") || !truthy("actions") {
        return Err(exception());
    }
    let mut errors = Vec::new();
    let mut conditions = Vec::new();
    for condition in entries(&data["conditions"]).ok_or_else(exception)? {
        if condition.is_null() {
            continue;
        }
        let Some([Value::String(key), Value::String(test), value, ..]) =
            condition.as_array().map(Vec::as_slice)
        else {
            return Err(PageError::Fallback("unusual Herald conditions"));
        };
        if !FIELDS.contains(&key.as_str()) {
            return Err(PageError::Fallback("a Herald field not ported"));
        }
        let value = match array_keys(value) {
            Some(keys) => Value::Array(keys),
            None => value.clone(),
        };
        errors.extend(condition_error(test, &value)?);
        conditions.push(ConditionValue {
            field: key.clone(),
            condition: test.clone(),
            value,
        });
    }
    let mut actions = Vec::new();
    for action in entries(&data["actions"]).ok_or_else(exception)? {
        if action.is_null() {
            continue;
        }
        let Some(items) = action.as_array() else {
            return Err(PageError::Fallback("unusual Herald actions"));
        };
        let Some(Value::String(key)) = items.first() else {
            return Err(PageError::Fallback("unusual Herald actions"));
        };
        let Some((_, list)) = ACTIONS.iter().find(|(known, _)| known == key) else {
            return Err(PageError::Fallback("a Herald action not ported"));
        };
        let target = items.get(1).cloned().unwrap_or(Value::Null);
        let target = if *list {
            Value::Array(array_keys(&target).ok_or(PageError::Fallback(
                "Herald action targets which aren't lists",
            ))?)
        } else {
            target
        };
        actions.push(ActionValue {
            action: key.clone(),
            target,
        });
    }
    Ok((conditions, actions, errors))
}

/// `GET|POST /herald/edit/`: a new rule.
pub(crate) async fn create<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    handle(state.as_ref(), viewer, None, (method, uri, headers, body)).await
}

/// `GET|POST /herald/edit/{id}/`.
pub(crate) async fn edit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    // `(?:(?P<id>[1-9]\d*)/)?`.
    let id: i64 = Some(id.as_str())
        .filter(|id| {
            id.starts_with(|first: char| ('1'..='9').contains(&first))
                && id.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|id| id.parse().ok())
        .ok_or(PageError::NotFound)?;
    handle(
        state.as_ref(),
        viewer,
        Some(id),
        (method, uri, headers, body),
    )
    .await
}

/// A new rule (`content_type` and `rule_type`), or the dialog PHP
/// explains a bad choice with.
fn new_rule<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
    uri: &Uri,
) -> Result<Result<Edited, Response>, PageError> {
    let content_type = request.str("content_type").unwrap_or_default().to_owned();
    let rule_type = request.str("rule_type").unwrap_or_default().to_owned();
    if super::rule_type_name(&rule_type).is_none() {
        let paragraph =
            format!("The selected rule type (\"{rule_type}\") is not recognized by Herald.");
        return Ok(Err(refusal(
            state,
            viewer,
            uri,
            "Invalid Rule Type",
            paragraph,
        )));
    }
    let Some(adapter) = ADAPTERS
        .iter()
        .find(|adapter| adapter.content_type == content_type)
    else {
        let paragraph =
            format!("The selected content type (\"{content_type}\") is not recognized by Herald.");
        return Ok(Err(refusal(
            state,
            viewer,
            uri,
            "Invalid Content Type",
            paragraph,
        )));
    };
    if !adapter.supports(&rule_type) {
        let paragraph = format!(
            "The selected rule type (\"{rule_type}\") is not supported by the selected \
             content type (\"{content_type}\")."
        );
        return Ok(Err(refusal(
            state,
            viewer,
            uri,
            "Rule/Content Mismatch",
            paragraph,
        )));
    }
    if rule_type == "object" {
        return Err(PageError::Fallback("an object Herald rule"));
    }
    if content_type != REVISIONS {
        return Err(PageError::Fallback(
            "a Herald rule of an adapter not ported",
        ));
    }
    Ok(Ok(Edited {
        target: Target::New {
            content_type: content_type.clone(),
            rule_type: rule_type.clone(),
        },
        id: None,
        name: String::new(),
        content_type,
        rule_type,
        match_all: true,
        repetition_policy: "every".to_owned(),
        rule_json: "{\"conditions\": [], \"actions\": []}".to_owned(),
    }))
}

async fn handle<S: WebState>(
    state: &S,
    viewer: WebUser,
    id: Option<i64>,
    (method, uri, headers, body): (Method, Uri, HeaderMap, Bytes),
) -> Result<Response, PageError> {
    viewer.require_application(state, APPLICATION).await?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let edited = if let Some(id) = id {
        existing(state, &viewer, id).await?
    } else {
        match new_rule(state, &viewer, &request, &uri)? {
            Ok(edited) => edited,
            Err(dialog) => return Ok(dialog),
        }
    };
    if edited.rule_type == "global" && !can_manage_global(state, &viewer).await? {
        return Err(PageError::PolicyDenied);
    }
    let mut page = form_page(state, &viewer, &uri, &edited);
    if !request.is_post() {
        return Ok(Page(page).into_response());
    }
    web_edit::check_form_post(state, &viewer, &request)?;
    if !request
        .str("save")
        .is_some_and(phab_core::php::is_truthy_str)
    {
        return Ok(Page(page).into_response());
    }

    // saveRule().
    let name = request.str("name").unwrap_or_default().to_owned();
    let match_all = request.str("must_match") == Some("all");
    let repetition = request
        .str("repetition_policy")
        .filter(|policy| REPETITION.iter().any(|(key, _)| key == policy))
        .unwrap_or("every")
        .to_owned();
    let mut errors = Vec::new();
    let mut name_error = None;
    if name.is_empty() {
        name_error = Some("Required");
        errors.push("Rule must have a name.".to_owned());
    }
    let rule_json = request.str("rule");
    let (conditions, actions, condition_errors) = posted_rule(rule_json)?;
    errors.extend(condition_errors);
    if errors.is_empty() {
        let change = Change::Save {
            name: name.clone(),
            components: Components {
                match_all,
                conditions,
                actions,
                repetition_policy: repetition.clone(),
            },
        };
        match apply_validated(state, &viewer, edited.target.clone(), change).await {
            Ok(outcome) => {
                return Ok(request.redirect(state.config(), &format!("/H{}", outcome.id)));
            }
            Err(Refusal::Page(error)) => return Err(error),
            Err(Refusal::Invalid(message)) => errors.push(message),
        }
    }
    // The form again, as posted.
    page.errors = errors;
    page.name_error = name_error;
    page.name = name;
    page.match_all = match_all;
    for option in &mut page.repetition {
        option.2 = option.0 == repetition;
    }
    page.rule_json = rule_json.unwrap_or_default().to_owned();
    Ok(Page(page).into_response())
}

fn form_page<S: WebState>(state: &S, viewer: &WebUser, uri: &Uri, edited: &Edited) -> EditPage {
    let content_type_name = super::adapter_name(&edited.content_type).unwrap_or("");
    let title = match edited.id {
        Some(_) => format!("Edit Herald Rule: {}", edited.name),
        None => format!("Create Herald Rule: {content_type_name}"),
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    let actions = ACTIONS
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| {
            // `getActions($rule_type)`: the self actions only in personal
            // rules, tags only in global ones.
            let personal = edited.rule_type == "personal";
            if key.contains(".self") {
                personal
            } else if key.starts_with("projects.") {
                !personal
            } else {
                true
            }
        })
        .collect();
    EditPage {
        shell: Shell::for_user(state.config(), viewer, &title, current),
        title,
        action: current.to_owned(),
        errors: Vec::new(),
        name_error: None,
        name: edited.name.clone(),
        content_type: edited.content_type.clone(),
        rule_type: edited.rule_type.clone(),
        rule_type_name: super::rule_type_name(&edited.rule_type).unwrap_or(""),
        content_type_name,
        match_all: edited.match_all,
        repetition: REPETITION
            .iter()
            .map(|(key, label)| (*key, *label, *key == edited.repetition_policy))
            .collect(),
        rule_json: edited.rule_json.clone(),
        fields: FIELDS,
        actions,
        cancel: edited
            .id
            .map_or_else(|| "/herald/".to_owned(), |id| format!("/H{id}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_posted_rules_like_php() {
        let (conditions, actions, errors) = posted_rule(Some(
            r#"{"conditions": [null, ["cc", "any", {"PHID-USER-a": "a", "7": "x"}],
                ["differential.revision.title", "regexp", "nope"]],
               "actions": {"3": ["subscribers.add", {"PHID-USER-b": "b"}], "4": ["nothing"]}}"#,
        ))
        .unwrap();
        assert_eq!(conditions[0].value, json!(["PHID-USER-a", 7]));
        assert_eq!(actions[0].target, json!(["PHID-USER-b"]));
        assert_eq!(actions[1].target, Value::Null);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("The regular expression \"nope\" is not valid."));
        for invalid in [
            "x",
            "[]",
            r#"{"conditions": [], "actions": [["nothing"]]}"#,
            r#"{"conditions": [["space", "is", "x"]], "actions": [["nothing"]]}"#,
            r#"{"conditions": [["cc", "exists", null]], "actions": [["flag", 1]]}"#,
            r#"{"conditions": [["cc", "exists", null]], "actions": [["email.other", null]]}"#,
        ] {
            assert!(posted_rule(Some(invalid)).is_err(), "{invalid}");
        }
        assert_eq!(
            condition_error("regexp-pair", &json!("[\"@a@\"]")).unwrap(),
            Some(
                "The regular expression pair \"[\"@a@\"]\" must have exactly two elements."
                    .to_owned()
            )
        );
        assert_eq!(condition_error("regexp", &json!("@^a@")).unwrap(), None);
        assert_eq!(
            condition_error("wat", &json!(null)).unwrap(),
            Some("Unknown condition \"wat\"!".to_owned())
        );
    }
}
