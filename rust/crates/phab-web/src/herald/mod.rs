//! Herald pages (`PhabricatorHeraldApplication`), read-only: rules
//! (`/H<id>`), the rule list, and transcripts. Editing, disabling and the
//! test console link to PHP.
//!
//! Rules and transcripts render through their adapter's fields and
//! actions (`HeraldAdapter::renderRuleAsText()`, `HeraldField`,
//! `HeraldAction`). Only the revision adapter (`differential`) is ported:
//! a rule or transcript of another adapter, or naming a field, action or
//! effect not listed here, goes to PHP.

mod create;
mod edit;
mod forms;
mod lists;
mod rule;
mod rule_edit;
mod test_console;
mod transcript;
mod webhooks;

use std::collections::HashMap;

use phab_app_herald::web::{self as herald, RuleFilter};
use phab_core::Phid;
use phab_db::phpser::Value as Php;
use phab_policy::application;

pub(crate) use create::new_rule;
pub(crate) use forms::disable;
pub(crate) use lists::{
    cursor, rules as rule_list, rules_query as rule_query, transcripts as transcript_list,
};
pub(crate) use rule::show as rule;
pub(crate) use rule_edit::{create as rule_create, edit as rule_edit};
pub(crate) use test_console::test_console;
pub(crate) use transcript::{show as transcript, show_view as transcript_view};
pub(crate) use webhooks::{webhooks as webhook_list, webhooks_query as webhook_query};

use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Handle, Scope};
use crate::state::WebState;

/// `PhabricatorHeraldApplication`.
pub(crate) const APPLICATION: &str = application::HERALD;

/// The adapter whose rules and transcripts the pages render.
const REVISIONS: &str = "differential";

/// The ID of a rule monogram (`/H(?P<id>[1-9]\d*)`).
pub(crate) fn parse_rule_monogram(monogram: &str) -> Option<i64> {
    monogram
        .strip_prefix('H')
        .filter(|digits| {
            !digits.is_empty()
                && !digits.starts_with('0')
                && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|digits| digits.parse().ok())
}

/// `getAdapterContentName()` of every adapter, by content type.
pub(crate) fn adapter_name(content_type: &str) -> Option<&'static str> {
    Some(match content_type {
        "commit" => "Commits",
        "HeraldPreCommitRefAdapter" => "Commit Hook: Branches/Tags/Bookmarks",
        "HeraldPreCommitContentAdapter" => "Commit Hook: Commit Content",
        "PhabricatorProjectHeraldAdapter" => "Projects",
        "PhabricatorMailOutboundMailHeraldAdapter" => "Outbound Mail",
        "PhabricatorCalendarEventHeraldAdapter" => "Calendar Events",
        "differential.diff" => "Differential Diffs",
        "differential" => "Differential Revisions",
        "HeraldRuleAdapter" => "Herald Rules",
        _ => return None,
    })
}

/// `HeraldRuleTypeConfig::getRuleTypeMap()`.
pub(crate) fn rule_type_name(rule_type: &str) -> Option<&'static str> {
    Some(match rule_type {
        "personal" => "Personal",
        "object" => "Object",
        "global" => "Global",
        _ => return None,
    })
}

/// `HeraldField::getHeraldFieldStandardType()`, or the value type of the
/// fields which define their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    TextList,
    TextMap,
    Phid,
    PhidList,
    Bool,
    /// `revision.status`: a PHID field whose values are status keys.
    Status,
    /// `contentsource`: a select of the content sources.
    ContentSource,
    /// `always`.
    Always,
}

/// The fields of the revision adapter (`getFieldNameMap()`), but custom
/// fields, spaces and "Another Herald rule".
const REVISION_FIELDS: &[(&str, &str, Kind)] = &[
    ("differential.revision.title", "Revision title", Kind::Text),
    (
        "differential.revision.summary",
        "Revision summary",
        Kind::Text,
    ),
    (
        "differential.revision.test-plan",
        "Revision test plan",
        Kind::Text,
    ),
    ("differential.revision.author", "Author", Kind::Phid),
    (
        "differential.revision.author.projects",
        "Author's projects",
        Kind::PhidList,
    ),
    (
        "differential.revision.reviewers",
        "Reviewers",
        Kind::PhidList,
    ),
    ("differential.revision.repository", "Repository", Kind::Phid),
    (
        "differential.revision.repository.projects",
        "Repository projects",
        Kind::PhidList,
    ),
    ("revision.status", "Revision status", Kind::Status),
    (
        "differential.revision.diff.affected",
        "Affected files",
        Kind::TextList,
    ),
    (
        "differential.revision.diff.content",
        "Changed file content",
        Kind::TextMap,
    ),
    (
        "differential.revision.diff.new",
        "Added file content",
        Kind::TextMap,
    ),
    (
        "differential.revision.diff.old",
        "Removed file content",
        Kind::TextMap,
    ),
    ("cc", "Subscribers", Kind::PhidList),
    ("projects", "Project tags", Kind::PhidList),
    ("projects.added", "Project tags added", Kind::PhidList),
    ("projects.removed", "Project tags removed", Kind::PhidList),
    ("herald.acting-user", "Acting user", Kind::Phid),
    ("contentsource", "Content source", Kind::ContentSource),
    ("always", "Always", Kind::Always),
    ("new-object", "Is newly created", Kind::Bool),
    ("comment.content", "Comment content", Kind::TextList),
];

fn field(key: &str) -> Option<(&'static str, Kind)> {
    REVISION_FIELDS
        .iter()
        .find(|(field, _, _)| *field == key)
        .map(|(_, name, kind)| (*name, *kind))
}

/// `getHeraldFieldName()` of a field of the revision adapter.
fn field_name(key: &str) -> Result<&'static str, PageError> {
    field(key)
        .map(|(name, _)| name)
        .ok_or(PageError::Fallback("a Herald field not ported"))
}

/// `HeraldAdapter::getConditionNameMap()`.
fn condition_name(condition: &str) -> Option<&'static str> {
    Some(match condition {
        "contains" => "contains",
        "!contains" => "does not contain",
        "is" => "is",
        "!is" => "is not",
        "isany" => "is any of",
        "true" => "is true",
        "false" => "is false",
        "!isany" => "is not any of",
        "all" => "include all of",
        "any" => "include any of",
        "none" => "include none of",
        "me" => "is myself",
        "!me" => "is not myself",
        "regexp" => "matches regexp",
        "!regexp" => "does not match regexp",
        "conditions" => "matches:",
        "!conditions" => "does not match:",
        "exists" => "exists",
        "!exists" => "does not exist",
        "unconditionally" | "never" => "",
        "regexp-pair" => "matches regexp pair",
        "bit" => "has bit",
        "!bit" => "lacks bit",
        _ => return None,
    })
}

/// `DifferentialRevisionStatus::getAll()` display names, by key.
fn status_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "needs-review" => "Needs Review",
        "needs-revision" => "Needs Revision",
        "changes-planned" => "Changes Planned",
        "accepted" => "Accepted",
        "published" => "Closed",
        "abandoned" => "Abandoned",
        "draft" => "Draft",
        _ => return None,
    })
}

/// `getSourceName()` of `PhabricatorContentSource::getAllContentSources()`.
fn content_source_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "bulk" => "Bulk Update",
        "daemon" => "Daemon",
        "conduit" => "Conduit",
        "legacy" => "Old World",
        "fax" => "Fax",
        "console" => "Console",
        "web" => "Web",
        "unknown" => "Unknown",
        "unittest" => "Unit Test",
        "email" => "Email",
        "herald" => "Herald",
        _ => return None,
    })
}

/// `(string)` of a scalar, the strings of an array's values concatenated
/// (`phutil_escape_html()` of a list).
fn text(value: &Php) -> String {
    match value {
        Php::Array(entries) => entries.iter().map(|(_, value)| text(value)).collect(),
        other => herald::php_string(other).unwrap_or_default(),
    }
}

/// `(array)$value`, as strings.
fn as_list(value: &Php) -> Vec<String> {
    match value {
        Php::Null => Vec::new(),
        Php::Array(entries) => entries.iter().map(|(_, value)| text(value)).collect(),
        other => vec![text(other)],
    }
}

/// The values of `value` which name objects, to load their handles:
/// PHIDs. Values like status keys are not.
fn phids_of(values: &[String]) -> Vec<Phid> {
    values
        .iter()
        .filter(|value| value.starts_with("PHID-"))
        .map(|value| Phid::new(value.as_str()))
        .collect()
}

/// The handles the pages show: those of `harbormaster::load_handles()`,
/// and Herald rules (`HeraldRulePHIDType`).
pub(crate) async fn load_handles<S: WebState>(
    state: &S,
    scope: &Scope,
    phids: &[Phid],
) -> Result<HashMap<Phid, Handle>, PageError> {
    let (rules, others): (Vec<Phid>, Vec<Phid>) = phids
        .iter()
        .cloned()
        .partition(|phid| phid.as_str().get(5..9) == Some("HRUL"));
    let mut handles = harbormaster::load_handles(state, scope, &others).await?;
    if rules.is_empty() {
        return Ok(handles);
    }
    let found = herald::load_rules(
        state.database(),
        &RuleFilter {
            phids: Some(&rules),
            ..RuleFilter::default()
        },
    )
    .await?;
    let visible = rule::visible_rules(state, scope, found.clone()).await?;
    for phid in rules {
        let handle = if let Some(rule) = visible.iter().find(|rule| rule.phid == phid) {
            let monogram = rule.monogram();
            Handle {
                full_name: format!("{monogram} {}", rule.name),
                uri: Some(format!("/{monogram}")),
                closed: rule.is_disabled,
                name: monogram,
            }
        } else {
            let name = if found.iter().any(|rule| rule.phid == phid) {
                "Restricted Herald Rule".to_owned()
            } else {
                "Unknown Object (Herald Rule)".to_owned()
            };
            Handle {
                name: name.clone(),
                full_name: name,
                uri: None,
                closed: false,
            }
        };
        handles.insert(phid, handle);
    }
    Ok(handles)
}

/// `renderHandleList()`: links separated by commas, or by line breaks
/// when not inline.
fn handle_list(handles: &HashMap<Phid, Handle>, values: &[String], inline: bool) -> String {
    values
        .iter()
        .map(|value| harbormaster::handle(handles, &Phid::new(value.as_str())).link_html())
        .collect::<Vec<_>>()
        .join(if inline { ", " } else { "<br />" })
}

/// The PHIDs a condition's value names.
fn condition_phids(key: &str, condition: &str, value: &Php) -> Vec<Phid> {
    match field(key) {
        Some((_, Kind::Phid | Kind::PhidList)) if !matches!(condition, "exists" | "!exists") => {
            phids_of(&as_list(value))
        }
        _ => Vec::new(),
    }
}

/// `HeraldField::renderConditionValue()`, as HTML.
fn condition_value_html(
    key: &str,
    condition: &str,
    value: &Php,
    handles: &HashMap<Phid, Handle>,
) -> Result<String, PageError> {
    let (_, kind) = field(key).ok_or(PageError::Fallback("a Herald field not ported"))?;
    let empty = matches!(condition, "exists" | "!exists");
    Ok(match kind {
        Kind::Bool | Kind::Always => String::new(),
        Kind::Text | Kind::TextList | Kind::TextMap => {
            if empty {
                String::new()
            } else {
                escape(&text(value))
            }
        }
        Kind::Phid | Kind::PhidList | Kind::Status => {
            if empty {
                return Ok(String::new());
            }
            let values = as_list(value);
            if values.is_empty() {
                "<em>None</em>".to_owned()
            } else if kind == Kind::Status {
                escape(
                    &values
                        .iter()
                        .map(|value| status_name(value).unwrap_or(value).to_owned())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            } else {
                handle_list(handles, &values, true)
            }
        }
        Kind::ContentSource => {
            let value = text(value);
            escape(content_source_name(&value).unwrap_or(&value))
        }
    })
}

/// `HeraldFieldValue::renderTranscriptValue()` of a field value, as HTML.
fn transcript_value_html(
    key: &str,
    value: &Php,
    handles: &HashMap<Phid, Handle>,
) -> Result<String, PageError> {
    let (_, kind) = field(key).ok_or(PageError::Fallback("a Herald field not ported"))?;
    Ok(match kind {
        Kind::Bool | Kind::Always => {
            if phab_core_truthy(value) {
                "true".to_owned()
            } else {
                "false".to_owned()
            }
        }
        Kind::Text | Kind::TextList | Kind::TextMap => {
            let value = text(value);
            if value.is_empty() {
                "<em>None</em>".to_owned()
            } else if value.len() > 256 {
                format!(
                    "<textarea class=\"herald-field-value-transcript\" readonly>{}</textarea>",
                    escape(&value)
                )
            } else {
                escape(&value)
            }
        }
        Kind::Phid | Kind::PhidList | Kind::Status => {
            let values = as_list(value);
            if values.is_empty() {
                "<em>None</em>".to_owned()
            } else if kind == Kind::Status {
                escape(
                    &values
                        .iter()
                        .map(|value| status_name(value).unwrap_or(value).to_owned())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            } else {
                handle_list(handles, &values, false)
            }
        }
        Kind::ContentSource => {
            let value = text(value);
            escape(content_source_name(&value).unwrap_or(&value))
        }
    })
}

/// The PHIDs a field value of the object transcript names.
fn transcript_value_phids(key: &str, value: &Php) -> Vec<Phid> {
    match field(key) {
        Some((_, Kind::Phid | Kind::PhidList)) => phids_of(&as_list(value)),
        _ => Vec::new(),
    }
}

/// PHP's truthiness.
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

/// How an action of the revision adapter describes itself
/// (`renderActionDescription()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Description {
    /// A fixed sentence.
    Fixed(&'static str),
    /// A sentence ending with the targets: "Add reviewers: x, y."
    Targets(&'static str),
    /// A flag color: "Mark with Red flag."
    Flag(&'static str, &'static str),
}

/// What an action's effects say (`getActionEffectMap()` and
/// `renderActionEffectDescription()`), by family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effects {
    Subscriptions,
    Projects,
    Email,
    MustEncrypt,
    Reviewers,
    Nothing,
    /// Not ported: transcripts with these actions go to PHP.
    Other,
}

/// The actions of the revision adapter: key, `getHeraldActionName()`,
/// description and effects.
const ACTIONS: &[(&str, &str, Description, Effects)] = &[
    (
        "subscribers.add",
        "Add subscribers",
        Description::Targets("Add subscribers"),
        Effects::Subscriptions,
    ),
    (
        "subscribers.remove",
        "Remove subscribers",
        Description::Targets("Remove subscribers"),
        Effects::Subscriptions,
    ),
    (
        "subscribers.self.add",
        "Add me as a subscriber",
        Description::Fixed("Add rule author as subscriber."),
        Effects::Subscriptions,
    ),
    (
        "subscribers.self.remove",
        "Remove me as a subscriber",
        Description::Fixed("Remove rule author as subscriber."),
        Effects::Subscriptions,
    ),
    (
        "projects.add",
        "Add projects",
        Description::Targets("Add projects"),
        Effects::Projects,
    ),
    (
        "projects.remove",
        "Remove projects",
        Description::Targets("Remove projects"),
        Effects::Projects,
    ),
    (
        "email.other",
        "Send an email to",
        Description::Targets("Send an email to"),
        Effects::Email,
    ),
    (
        "email.self",
        "Send me an email",
        Description::Fixed("Send an email to rule author."),
        Effects::Email,
    ),
    (
        "email.must-encrypt",
        "Require secure email",
        Description::Fixed("Require mail content be transmitted only over secure channels."),
        Effects::MustEncrypt,
    ),
    (
        "differential.reviewers.add",
        "Add reviewers",
        Description::Targets("Add reviewers"),
        Effects::Reviewers,
    ),
    (
        "differential.reviewers.blocking",
        "Add blocking reviewers",
        Description::Targets("Add blocking reviewers"),
        Effects::Reviewers,
    ),
    (
        "differential.reviewers.self.add",
        "Add me as a reviewer",
        Description::Fixed("Add rule author as reviewer."),
        Effects::Reviewers,
    ),
    (
        "differential.reviewers.self.blocking",
        "Add me as a blocking reviewer",
        Description::Fixed("Add rule author as blocking reviewer."),
        Effects::Reviewers,
    ),
    (
        "nothing",
        "Do nothing",
        Description::Fixed("Do nothing."),
        Effects::Nothing,
    ),
    (
        "harbormaster.build",
        "Run build plans",
        Description::Targets("Run build plans"),
        Effects::Other,
    ),
    (
        "reviewhelper.request",
        "Request AI review",
        Description::Fixed("Request AI review."),
        Effects::Other,
    ),
    (
        "flag",
        "Mark with flag",
        Description::Flag("Mark with", "flag."),
        Effects::Other,
    ),
    (
        "unflag",
        "Remove flag",
        Description::Flag("Remove", "flag."),
        Effects::Other,
    ),
];

fn action(key: &str) -> Option<(&'static str, Description, Effects)> {
    ACTIONS
        .iter()
        .find(|(action, _, _, _)| *action == key)
        .map(|(_, name, description, effects)| (*name, *description, *effects))
}

/// `PhabricatorFlagColor::getColorName()`.
fn flag_color(value: &Php) -> &'static str {
    const COLORS: [&str; 8] = [
        "Red",
        "Orange",
        "Yellow",
        "Green",
        "Blue",
        "Pink",
        "Purple",
        "Checkered",
    ];
    let index = match value {
        Php::Int(number) => usize::try_from(*number).ok(),
        Php::Str(bytes) => std::str::from_utf8(bytes)
            .ok()
            .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse().ok()),
        _ => None,
    };
    index
        .and_then(|index| COLORS.get(index).copied())
        .unwrap_or("Unknown")
}

/// The PHIDs an action's description names.
fn action_phids(key: &str, target: &Php) -> Vec<Phid> {
    match (action(key), target) {
        (Some((_, Description::Targets(_), _)), Php::Array(_)) => phids_of(&as_list(target)),
        _ => Vec::new(),
    }
}

/// `renderActionDescription()`, as HTML.
fn action_description_html(
    key: &str,
    target: &Php,
    handles: &HashMap<Phid, Handle>,
) -> Result<String, PageError> {
    let (_, description, _) =
        action(key).ok_or(PageError::Fallback("a Herald action not ported"))?;
    Ok(match description {
        Description::Fixed(sentence) => escape(sentence),
        Description::Targets(prefix) => {
            let list = match target {
                Php::Array(_) => handle_list(handles, &as_list(target), true),
                _ => "(Invalid List)".to_owned(),
            };
            format!("{}: {list}.", escape(prefix))
        }
        Description::Flag(prefix, suffix) => {
            format!("{prefix} {} {suffix}", flag_color(target))
        }
    })
}

/// `getHeraldActionName()`.
fn action_name(key: &str) -> Result<&'static str, PageError> {
    action(key)
        .map(|(name, _, _)| name)
        .ok_or(PageError::Fallback("a Herald action not ported"))
}

/// An effect of an applied action: icon, color and name
/// (`renderActionEffectIcon()`...), and its description as HTML.
struct EffectView {
    icon: Option<&'static str>,
    color: Option<&'static str>,
    name: Option<&'static str>,
    description_html: Option<String>,
}

/// English plurals of the effect descriptions
/// (`PhabricatorUSEnglishTranslation`): one target, or several.
fn plural(count: usize, one: &str, many: &str, list: &str) -> String {
    format!("{} {list}.", if count == 1 { one } else { many })
}

/// The PHIDs the description of an effect names.
fn effect_phids(data: &Php) -> Vec<Phid> {
    match data {
        Php::Array(_) => phids_of(&as_list(data)),
        _ => Vec::new(),
    }
}

/// `renderActionEffect*()` and `renderEffectDescription()` of an effect
/// logged by `logEffect()`.
#[allow(
    clippy::too_many_lines,
    reason = "the effect tables of the actions and their translations"
)]
fn effect(
    key: &str,
    kind: &str,
    data: &Php,
    handles: &HashMap<Phid, Handle>,
) -> Result<EffectView, PageError> {
    let (_, _, family) = action(key).ok_or(PageError::Fallback("a Herald action not ported"))?;
    if family == Effects::Other {
        return Err(PageError::Fallback(
            "the effects of a Herald action not ported",
        ));
    }
    let list = || match data {
        Php::Array(_) => handle_list(handles, &as_list(data), true),
        _ => "(Invalid List)".to_owned(),
    };
    let count = match data {
        Php::Array(entries) => Some(entries.len()),
        _ => None,
    };
    let counted = || count.ok_or(PageError::Fallback("an unusual Herald effect"));
    let spec: Option<(&str, &str, &str)> = match (family, kind) {
        (Effects::Subscriptions, "do.previously-unsubscribed") => {
            Some(("fa-minus-circle", "grey", "Previously Unsubscribed"))
        }
        (Effects::Subscriptions, "do.autosubscribed") => {
            Some(("fa-envelope", "grey", "Automatically Subscribed"))
        }
        (Effects::Subscriptions, "do.subscribed") => {
            Some(("fa-envelope", "green", "Added Subscribers"))
        }
        (Effects::Subscriptions, "do.unsubscribed") => {
            Some(("fa-minus-circle", "green", "Removed Subscribers"))
        }
        (Effects::Projects, "do.add-projects") => Some(("fa-briefcase", "green", "Added Projects")),
        (Effects::Projects, "do.remove-projects") => {
            Some(("fa-minus-circle", "green", "Removed Projects"))
        }
        (Effects::Email, "do.send") => Some(("fa-envelope", "green", "Sent Mail")),
        (Effects::Email, "do.force") => Some(("fa-envelope", "blue", "Forced Mail")),
        (Effects::MustEncrypt, "do.must-encrypt") => Some(("fa-shield", "blue", "Must Encrypt")),
        (Effects::Reviewers, "do.authors") => Some(("fa-user", "grey", "Revision Author")),
        (Effects::Reviewers, "do.add-reviewers") => Some(("fa-user", "green", "Added Reviewers")),
        (Effects::Reviewers, "do.add-blocking-reviewers") => {
            Some(("fa-user", "green", "Added Blocking Reviewers"))
        }
        (Effects::Nothing, "do.nothing") => Some(("fa-check", "grey", "Did Nothing")),
        (_, "do.standard.empty") => Some(("fa-ban", "grey", "No Targets")),
        (_, "do.standard.no-effect") => Some(("fa-circle-o", "grey", "No Effect")),
        (_, "do.standard.invalid") => Some(("fa-ban", "red", "Invalid Targets")),
        (_, "do.standard.unloadable") => Some(("fa-ban", "red", "Unloadable Targets")),
        (_, "do.standard.permission") => Some(("fa-lock", "red", "No Permission")),
        (_, "do.standard.invalid-action") => Some(("fa-ban", "red", "Invalid Action")),
        (_, "do.standard.wrong-rule-type") => Some(("fa-ban", "red", "Wrong Rule Type")),
        (_, "do.standard.forbidden") => {
            return Err(PageError::Fallback("a Herald effect forbidden by state"));
        }
        _ => None,
    };
    // The action's own description comes first.
    let own: Option<String> = match (family, kind) {
        (Effects::Nothing, _) => Some("Did nothing.".to_owned()),
        (Effects::Subscriptions, "do.previously-unsubscribed") => Some(plural(
            counted()?,
            "Delined to resubscribe a target because they previously unsubscribed:",
            "Declined to resubscribe targets because they previously unsubscribed:",
            &list(),
        )),
        (Effects::Subscriptions, "do.autosubscribed") => Some(plural(
            counted()?,
            "An automatically subscribed target was not affected:",
            "Automatically subscribed targets were not affected:",
            &list(),
        )),
        (Effects::Subscriptions, "do.subscribed") => Some(plural(
            counted()?,
            "Added a subscriber:",
            "Added subscribers:",
            &list(),
        )),
        (Effects::Subscriptions, "do.unsubscribed") => Some(plural(
            counted()?,
            "Removed a subscriber:",
            "Removed subscribers:",
            &list(),
        )),
        (Effects::Projects, "do.add-projects") => Some(plural(
            counted()?,
            "Added a project:",
            "Added projects:",
            &list(),
        )),
        (Effects::Projects, "do.remove-projects") => Some(plural(
            counted()?,
            "Removed a project:",
            "Removed projects:",
            &list(),
        )),
        (Effects::Email, "do.send") => Some(plural(
            counted()?,
            "Queued email to be delivered to target:",
            "Queued email to be delivered to targets:",
            &list(),
        )),
        (Effects::Email, "do.force") => Some(plural(
            counted()?,
            "Queued email to be delivered to target, ignoring notification preferences:",
            "Queued email to be delivered to targets, ignoring notification preferences:",
            &list(),
        )),
        (Effects::MustEncrypt, "do.must-encrypt") => Some(
            "Made it a requirement that mail content be transmitted only over secure channels."
                .to_owned(),
        ),
        (Effects::Reviewers, "do.authors") => Some(format!(
            "Declined to add revision author as reviewer: {}.",
            list()
        )),
        (Effects::Reviewers, "do.add-reviewers") => Some(plural(
            counted()?,
            "Added a reviewer:",
            "Added reviewers:",
            &list(),
        )),
        (Effects::Reviewers, "do.add-blocking-reviewers") => Some(plural(
            counted()?,
            "Added a blocking reviewer:",
            "Added blocking reviewers:",
            &list(),
        )),
        _ => None,
    };
    let description_html = match own {
        Some(own) => Some(own),
        None => match kind {
            "do.standard.empty" => Some("This action specifies no targets.".to_owned()),
            "do.standard.no-effect" => Some(match count {
                Some(count) if count > 0 => plural(
                    count,
                    "This action has no effect on a target:",
                    "This action has no effect on targets:",
                    &list(),
                ),
                _ => "This action has no effect.".to_owned(),
            }),
            "do.standard.invalid" => Some(format!(
                "{} target(s) are invalid or of the wrong type: {}.",
                counted()?,
                list()
            )),
            "do.standard.unloadable" => Some(format!(
                "{} target(s) could not be loaded: {}.",
                counted()?,
                list()
            )),
            "do.standard.permission" => Some(plural(
                counted()?,
                "A target does not have permission to see this object:",
                "Targets do not have permission to see this object:",
                &list(),
            )),
            "do.standard.invalid-action" => Some(format!(
                "No implementation is available for rule &quot;{}&quot;.",
                escape(&text(data))
            )),
            "do.standard.wrong-rule-type" => Some(format!(
                "This action does not support rules of type &quot;{}&quot;.",
                escape(&text(data))
            )),
            _ => None,
        },
    };
    Ok(EffectView {
        icon: spec.map(|spec| spec.0),
        color: spec.map(|spec| spec.1),
        name: spec.map(|spec| spec.2),
        description_html,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_render_like_php() {
        let handles = HashMap::new();
        assert_eq!(
            condition_value_html(
                "revision.status",
                "isany",
                &Php::list([Php::str("accepted"), Php::str("draft")]),
                &handles
            )
            .unwrap(),
            "Accepted, Draft"
        );
        assert_eq!(
            condition_value_html(
                "differential.revision.title",
                "contains",
                &Php::str("<b>"),
                &handles
            )
            .unwrap(),
            "&lt;b&gt;"
        );
        assert_eq!(
            condition_value_html("cc", "exists", &Php::Null, &handles).unwrap(),
            ""
        );
        assert_eq!(
            condition_value_html("cc", "any", &Php::list([]), &handles).unwrap(),
            "<em>None</em>"
        );
        assert_eq!(
            condition_value_html("contentsource", "is", &Php::str("web"), &handles).unwrap(),
            "Web"
        );
        assert!(condition_value_html("space", "isany", &Php::Null, &handles).is_err());
        assert_eq!(
            transcript_value_html("new-object", &Php::Bool(false), &handles).unwrap(),
            "false"
        );
        assert_eq!(
            transcript_value_html(
                "differential.revision.diff.affected",
                &Php::list([Php::str("/a"), Php::str("/b")]),
                &handles
            )
            .unwrap(),
            "/a/b"
        );
    }

    #[test]
    fn effects_use_english_plurals() {
        let handles = HashMap::new();
        let one = effect(
            "subscribers.add",
            "do.subscribed",
            &Php::list([Php::str("x")]),
            &handles,
        )
        .unwrap();
        assert_eq!(one.name, Some("Added Subscribers"));
        assert!(
            one.description_html
                .unwrap()
                .starts_with("Added a subscriber: ")
        );
        let none = effect(
            "email.other",
            "do.standard.no-effect",
            &Php::list([]),
            &handles,
        )
        .unwrap();
        assert_eq!(
            none.description_html.as_deref(),
            Some("This action has no effect.")
        );
        let nothing = effect("nothing", "do.standard.empty", &Php::Null, &handles).unwrap();
        assert_eq!(nothing.description_html.as_deref(), Some("Did nothing."));
        assert_eq!(nothing.name, Some("No Targets"));
        assert!(effect("harbormaster.build", "do.build", &Php::Null, &handles).is_err());
        assert_eq!(flag_color(&Php::Int(6)), "Purple");
        assert_eq!(flag_color(&Php::Int(9)), "Unknown");
    }
}
