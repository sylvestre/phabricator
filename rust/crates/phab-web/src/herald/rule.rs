//! `/H<id>`: a Herald rule (`HeraldRuleViewController`), with what it
//! does as `HeraldAdapter::renderRuleAsText()` says it, and its timeline.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use askama::Template;
use axum::http::Uri;
use phab_app_harbormaster::web::{Transaction, TransactionTable};
use phab_app_herald::engine::{ActionRecord, Condition};
use phab_app_herald::web::{self as herald, RuleFilter, RuleRow};
use phab_core::{Phid, PhidType};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, PolicySubject};
use serde_json::Value;

use super::{
    APPLICATION, REVISIONS, action_description_html, action_phids, condition_name, condition_phids,
    condition_value_html, field_name, load_handles, rule_type_name,
};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Action, Handle, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;

/// `PHUICurtainExtension` of subscribers shows this many.
const VISIBLE_SUBSCRIBERS: usize = 8;

#[derive(Template)]
#[template(path = "herald_rule.html")]
pub(crate) struct RulePage {
    shell: Shell,
    id: i64,
    name: String,
    /// Icon, color and label of the header's status.
    status: (&'static str, &'static str, &'static str),
    policy: &'static str,
    actions: Vec<Action>,
    subscribers_html: Vec<String>,
    /// The "View All" link of a long subscriber list, and its count.
    all_subscribers: Option<(String, usize)>,
    rule_type: &'static str,
    author_html: Option<String>,
    applies_to: &'static str,
    match_text: &'static str,
    conditions_html: Vec<String>,
    action_text: &'static str,
    actions_html: Vec<String>,
    timeline: Vec<Entry>,
}

/// Whether the viewer may edit the rule (`CAN_EDIT`): the global rules
/// capability for global rules, the author for personal ones.
pub(crate) async fn can_edit<S: WebState>(
    state: &S,
    scope: &Scope,
    rule: &RuleRow,
) -> Result<bool, PageError> {
    let policy = match rule.rule_type.as_str() {
        "global" => herald::global_rules_policy(state.config()),
        "personal" => rule.author_phid.as_str().to_owned(),
        _ => return Err(PageError::Fallback("an object Herald rule")),
    };
    let subject = PolicySubject::new(Some(rule.phid.clone()), ObjectKind::Other)
        .with_policy(Capability::EDIT, policy);
    let viewer = scope.viewer();
    let facts = phab_policy::loader::load_facts(
        state.database(),
        state.config(),
        &viewer,
        &[&subject],
        &NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    phab_policy::has_capability(&viewer, &subject, Capability::EDIT, &facts)
        .map_err(harbormaster::internal)
}

/// `HeraldRuleQuery::validateRuleAuthors()`.
pub(crate) async fn has_valid_author<S: WebState>(
    state: &S,
    rule: &RuleRow,
) -> Result<bool, PageError> {
    if rule.rule_type != "personal" {
        return Ok(true);
    }
    let users = phab_auth::user::load_users_by_phid(
        state.database(),
        std::slice::from_ref(&rule.author_phid),
    )
    .await?;
    Ok(users
        .iter()
        .any(|user| user.phid == rule.author_phid && user.is_activated(state.config())))
}

/// The rules of `rules` the viewer can see (`HeraldRuleQuery`): object
/// rules need their project or repository to be visible.
pub(crate) async fn visible_rules<S: WebState>(
    state: &S,
    scope: &Scope,
    rules: Vec<RuleRow>,
) -> Result<Vec<RuleRow>, PageError> {
    let triggers: Vec<Phid> = rules
        .iter()
        .filter(|rule| rule.is_object_rule())
        .filter_map(|rule| rule.trigger_object_phid.clone())
        .collect();
    let triggers: HashSet<Phid> = harbormaster::load_handles(state, scope, &triggers)
        .await?
        .into_iter()
        .filter(|(_, handle)| handle.uri.is_some())
        .map(|(phid, _)| phid)
        .collect();
    let visible: HashSet<Phid> = phab_app_herald::visible(
        state.database(),
        state.config(),
        &scope.viewer(),
        rules.iter().map(RuleRow::summary).collect(),
        &triggers,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?
    .into_iter()
    .map(|rule| rule.phid)
    .collect();
    Ok(rules
        .into_iter()
        .filter(|rule| visible.contains(&rule.phid))
        .collect())
}

/// `HeraldRuleTransaction` titles, and the core ones rules have when they
/// are created (`shouldHide()`).
fn title(transaction: &Transaction, author: &str) -> Result<Option<String>, PageError> {
    let is_create = transaction.metadata_flag("core.create");
    let old_is_empty = match &transaction.old_value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Number(number) => number.as_i64() == Some(0),
        Value::Bool(_) => false,
    };
    let kind = transaction.transaction_type.as_str();
    if is_create && transaction.metadata_flag("core.default") {
        return Ok(None);
    }
    if is_create && kind != "core:create" && old_is_empty {
        return Ok(None);
    }
    let value = |value: &Value| match value {
        Value::String(text) => Ok(escape(text)),
        _ => Err(PageError::Fallback("an unusual Herald rule transaction")),
    };
    Ok(Some(match kind {
        "core:create" => format!("{author} created this object."),
        "herald:name" => format!(
            "{author} renamed this rule from &quot;{}&quot; to &quot;{}&quot;.",
            value(&transaction.old_value)?,
            value(&transaction.new_value)?
        ),
        "herald:disable" => {
            if phab_core::php::is_truthy(&transaction.new_value) {
                format!("{author} disabled this rule.")
            } else {
                format!("{author} enabled this rule.")
            }
        }
        // The change shows as a JSON diff on PHP's detail page.
        "herald:edit" => format!(
            "{author} edited this rule. <a href=\"/transactions/detail/{}/\">Show Details</a>",
            escape(transaction.phid.as_str())
        ),
        _ => return Err(PageError::Fallback("a Herald rule transaction not ported")),
    }))
}

/// `PhabricatorSubscriptionsCurtainExtension`: the viewer first, then
/// visible objects, users before projects, by name.
fn sorted_subscribers(
    viewer: &Phid,
    subscribers: &[Phid],
    handles: &HashMap<Phid, Handle>,
) -> Vec<Phid> {
    let mut sorted = subscribers.to_vec();
    sorted.sort_by_cached_key(|phid| {
        let handle = harbormaster::handle(handles, phid);
        let complete = handle.uri.is_some();
        let type_order = match phid.phid_type() {
            PhidType::USER => 0,
            PhidType::PROJECT => 1,
            _ => 2,
        };
        (
            phid != viewer,
            !complete,
            type_order,
            if complete {
                handle.full_name.to_lowercase()
            } else {
                String::new()
            },
        )
    });
    sorted
}

/// The subscribers the curtain shows, as links, and the link to all of
/// them when there are too many.
fn subscriber_list(
    viewer: &Phid,
    rule: &Phid,
    subscribers: &[Phid],
    handles: &HashMap<Phid, Handle>,
) -> (Vec<String>, Option<(String, usize)>) {
    let mut subscribers = sorted_subscribers(viewer, subscribers, handles);
    let count = subscribers.len();
    let all = (count > VISIBLE_SUBSCRIBERS).then(|| {
        subscribers.truncate(VISIBLE_SUBSCRIBERS - 1);
        (format!("/subscriptions/list/{rule}/"), count)
    });
    let links = subscribers
        .iter()
        .map(|phid| harbormaster::handle(handles, phid).link_html())
        .collect();
    (links, all)
}

/// `renderRuleAsText()`: the conditions and actions, as HTML.
fn rule_text(
    conditions: &[Condition],
    actions: &[ActionRecord],
    handles: &HashMap<Phid, Handle>,
) -> Result<(Vec<String>, Vec<String>), PageError> {
    let conditions = conditions
        .iter()
        .map(|condition| {
            let name = field_name(&condition.field)?;
            let condition_text = condition_name(&condition.condition)
                .ok_or(PageError::Fallback("a Herald condition not ported"))?;
            let value = condition_value_html(
                &condition.field,
                &condition.condition,
                &condition.value,
                handles,
            )?;
            Ok(format!(
                "{} {} {value}",
                escape(name),
                escape(condition_text)
            ))
        })
        .collect::<Result<Vec<_>, PageError>>()?;
    let actions = actions
        .iter()
        .map(|action| action_description_html(&action.action, &action.target, handles))
        .collect::<Result<Vec<_>, PageError>>()?;
    Ok((conditions, actions))
}

/// `buildCurtain()`: editing and disabling, which PHP does.
fn curtain(rule: &RuleRow, can_edit: bool) -> Vec<Action> {
    let id = rule.id;
    let (disable_uri, disable_icon, disable_name) = if rule.is_disabled {
        (
            format!("/herald/disable/{id}/enable/"),
            "fa-check",
            "Enable Rule",
        )
    } else {
        (
            format!("/herald/disable/{id}/disable/"),
            "fa-ban",
            "Disable Rule",
        )
    };
    vec![
        Action {
            name: "Edit Rule",
            icon: "fa-pencil",
            uri: format!("/herald/edit/{id}/"),
            enabled: can_edit,
        },
        Action {
            name: disable_name,
            icon: disable_icon,
            uri: disable_uri,
            enabled: can_edit,
        },
    ]
}

/// `GET /H{id}`.
pub(crate) async fn show<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    id: i64,
    uri: Uri,
) -> Result<Page<RulePage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let state = state.as_ref();
    let database = state.database();
    let rule = herald::load_rules(
        database,
        &RuleFilter {
            ids: Some(&[id]),
            ..RuleFilter::default()
        },
    )
    .await?
    .pop()
    .ok_or(PageError::NotFound)?;
    // Their edit policy is their object's, and PHP hides those whose
    // object the viewer can't see.
    if rule.is_object_rule() {
        return Err(PageError::Fallback("an object Herald rule"));
    }
    let scope = Scope::new(state, &viewer);
    // The adapter's application is hidden from the viewer: PHP's query
    // rejects the rule, which raises a policy exception.
    let rule = visible_rules(state, &scope, vec![rule])
        .await?
        .pop()
        .ok_or(PageError::PolicyDenied)?;
    if rule.content_type != REVISIONS {
        return Err(PageError::Fallback(
            "a Herald rule of an adapter not ported",
        ));
    }
    let valid_author = has_valid_author(state, &rule).await?;
    let (conditions, rule_actions) = herald::load_conditions_and_actions(database, rule.id).await?;
    let subscribers = herald::subscribers(database, &rule.phid).await?;
    if subscribers.len() > 100 {
        return Err(PageError::Fallback("a Herald rule with many subscribers"));
    }

    let mut phids: Vec<Phid> = conditions
        .iter()
        .flat_map(|condition| {
            condition_phids(&condition.field, &condition.condition, &condition.value)
        })
        .chain(
            rule_actions
                .iter()
                .flat_map(|action| action_phids(&action.action, &action.target)),
        )
        .chain(subscribers.iter().cloned())
        .collect();
    let is_personal = rule.rule_type == "personal";
    if is_personal {
        phids.push(rule.author_phid.clone());
    }
    let handles = load_handles(state, &scope, &phids).await?;
    let (conditions_html, actions_html) = rule_text(&conditions, &rule_actions, &handles)?;
    let (subscribers_html, all_subscribers) =
        subscriber_list(&viewer.user.phid, &rule.phid, &subscribers, &handles);
    let actions = curtain(&rule, can_edit(state, &scope, &rule).await?);
    let table = TransactionTable::HeraldRule;
    let timeline = harbormaster::timeline(state, &scope, table, &rule.phid, title).await?;
    let status = if rule.is_disabled {
        ("fa-ban", "red", "Disabled")
    } else if !valid_author {
        ("fa-user", "red", "Author Not Active")
    } else {
        ("fa-check", "bluegrey", "Active")
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(RulePage {
        shell: Shell::for_user(state.config(), &viewer, &rule.name, current),
        id: rule.id,
        status,
        policy: if state.config().allow_public() {
            "Public (No Login Required)"
        } else {
            "All Users"
        },
        actions,
        subscribers_html,
        all_subscribers,
        rule_type: rule_type_name(&rule.rule_type).unwrap_or(""),
        author_html: is_personal
            .then(|| harbormaster::handle(&handles, &rule.author_phid).link_html()),
        applies_to: super::adapter_name(&rule.content_type).unwrap_or(""),
        match_text: if rule.must_match_all {
            "When all of these conditions are met:"
        } else {
            "When any of these conditions are met:"
        },
        conditions_html,
        action_text: match rule.repetition_policy.as_str() {
            "first" => "Take these actions the first time this rule matches:",
            "change" => "Take these actions if this rule did not match the last time:",
            _ => "Take these actions every time this rule matches:",
        },
        actions_html,
        timeline,
        name: rule.name,
    }))
}
