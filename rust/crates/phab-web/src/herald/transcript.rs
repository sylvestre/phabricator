//! `/herald/transcript/<id>/`: what Herald did to an object
//! (`HeraldTranscriptController`), as its rules, field values or profile.
//! The transactions view, which renders the object's transactions, is
//! PHP's.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_herald::web::{self as herald, ResultMap, Transcript};
use phab_core::Phid;
use phab_db::phpser::{Key, Value as Php};

use super::{
    APPLICATION, REVISIONS, as_list, condition_name, effect, effect_phids, field_name,
    load_handles, phids_of, text, transcript_value_html, transcript_value_phids,
};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Handle, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// A line of a status list (`PHUIStatusItemView`).
pub(crate) struct StatusItem {
    icon: Option<&'static str>,
    color: Option<&'static str>,
    /// The target, as HTML.
    target_html: String,
    /// The note, as HTML.
    note_html: String,
}

/// An action box of a rule.
pub(crate) struct ActionBox {
    name: String,
    items: Vec<StatusItem>,
}

/// A rule of the rule transcript.
pub(crate) struct RuleBox {
    id: String,
    name: String,
    applied: bool,
    conditions: Vec<StatusItem>,
    result: StatusItem,
    actions: Vec<ActionBox>,
}

/// A row of the profile.
pub(crate) struct ProfileRow {
    kind: String,
    what_html: String,
    cost: String,
    count: String,
}

/// What the page shows under its header.
pub(crate) enum Content {
    /// The details were garbage collected.
    Old,
    Rules(Vec<RuleBox>),
    /// Names and values, as HTML.
    Fields(Vec<(String, String)>),
    /// `None` for older transcripts without profile.
    Profile(Option<Vec<ProfileRow>>),
}

#[derive(Template)]
#[template(path = "herald_transcript.html")]
pub(crate) struct TranscriptPage {
    shell: Shell,
    id: i64,
    dry_run: bool,
    view: &'static str,
    has_transactions: bool,
    content: Content,
}

/// `HeraldConditionResult`: icon, color and name of a code.
fn condition_result(code: &str) -> Result<(&'static str, &'static str, &'static str), PageError> {
    Ok(match code {
        "matched" => ("fa-check", "green", "Passed"),
        "failed" => ("fa-times", "red", "Failed"),
        "object-state" => ("fa-ban", "indigo", "Forbidden"),
        "invalid" => ("fa-exclamation-triangle", "yellow", "Invalid"),
        "recursion" => ("fa-exclamation-triangle", "red", "Recursion"),
        "exception" => ("fa-exclamation-triangle", "red", "Exception"),
        "unknown" => ("fa-question", "grey", "Unknown"),
        _ => return Err(PageError::Fallback("a Herald result code not ported")),
    })
}

/// `HeraldRuleResult`: icon, color, name and description of a code, and
/// whether it applies the actions.
#[allow(clippy::too_many_lines, reason = "the table of `HeraldRuleResult`")]
fn rule_result(
    code: &str,
) -> Result<(&'static str, &'static str, &'static str, &'static str, bool), PageError> {
    let failed = ("fa-times-circle", "red");
    Ok(match code {
        "any-match" => (
            "fa-check-circle",
            "green",
            "Matched",
            "Any condition matched.",
            true,
        ),
        "all-match" => (
            "fa-check-circle",
            "green",
            "Matched",
            "All conditions matched.",
            true,
        ),
        "any-failed" => (
            failed.0,
            failed.1,
            "Failed",
            "Not all conditions matched.",
            false,
        ),
        "all-failed" => (
            failed.0,
            failed.1,
            "Failed",
            "No conditions matched.",
            false,
        ),
        "last-match" => (
            failed.0,
            failed.1,
            "Failed",
            "This rule matched, but did not take any actions because it is configured to act \
             only if it did not match the last time.",
            false,
        ),
        "version" => (
            failed.0,
            failed.1,
            "Version Issue",
            "Rule could not be processed because it was created with a newer version of Herald.",
            false,
        ),
        "empty" => (
            failed.0,
            failed.1,
            "Empty",
            "Rule failed automatically because it has no conditions.",
            false,
        ),
        "owner" => (
            failed.0,
            failed.1,
            "Rule Owner",
            "Rule failed automatically because it is a personal rule and its owner is invalid \
             or disabled.",
            false,
        ),
        "view-policy" => (
            failed.0,
            failed.1,
            "View Policy",
            "Rule failed automatically because it is a personal rule and its owner does not \
             have permission to view the object.",
            false,
        ),
        "object-rule" => (
            failed.0,
            failed.1,
            "Object Rule",
            "Rule failed automatically because it is an object rule which is not relevant for \
             this object.",
            false,
        ),
        "exception" => (
            failed.0,
            failed.1,
            "Exception",
            "Rule failed because an exception occurred.",
            false,
        ),
        "evaluation-exception" => (
            failed.0,
            failed.1,
            "Exception",
            "Rule failed because an exception occurred while evaluating it.",
            false,
        ),
        "unknown" => (
            failed.0,
            failed.1,
            "Unknown",
            "Rule evaluation result is unknown.",
            false,
        ),
        "already-applied" => (
            failed.0,
            failed.1,
            "Already Applied",
            "This rule is only supposed to be repeated a single time, and it has already been \
             applied.",
            false,
        ),
        "object-state" => (
            "fa-ban",
            "indigo",
            "Forbidden",
            "Object state prevented rule evaluation.",
            false,
        ),
        "recursion" => (
            failed.0,
            failed.1,
            "Recursion",
            "This rule has a recursive dependency on itself and can not be evaluated.",
            false,
        ),
        _ => return Err(PageError::Fallback("a Herald result code not ported")),
    })
}

fn data_string(result: &ResultMap, key: &str) -> String {
    result
        .data
        .get(&Key::from(key))
        .map(text)
        .unwrap_or_default()
}

/// `phutil_escape_html_newlines()`.
fn escape_newlines(value: &str) -> String {
    escape(value).replace('\n', "<br />")
}

/// `newErrorView()`.
fn error_html(class: &str, message: &str) -> String {
    format!(
        "<strong>{}</strong>: {}",
        escape(class),
        escape_newlines(message)
    )
}

const UNKNOWN_ERROR: &str = "An unknown error occurred while evaluating this condition. No \
                             additional information is available.";

/// `HeraldConditionResult::newDetailsView()`.
fn condition_details(result: &ResultMap) -> Result<Option<String>, PageError> {
    Ok(match result.code.as_str() {
        "object-state" => return Err(PageError::Fallback("a Herald state reason")),
        "invalid" | "exception" => {
            let mut class = data_string(result, "exception.class");
            let mut message = data_string(result, "exception.message");
            if class.is_empty() {
                "Unknown Error".clone_into(&mut class);
            }
            if class == "HeraldInvalidConditionException" {
                "Invalid Condition".clone_into(&mut class);
            }
            if message.is_empty() {
                UNKNOWN_ERROR.clone_into(&mut message);
            }
            Some(error_html(&class, &message))
        }
        _ => None,
    })
}

/// The rule PHIDs of a recursion result's cycle.
fn cycle_phids(result: &ResultMap) -> Vec<String> {
    if result.code != "recursion" {
        return Vec::new();
    }
    result
        .data
        .get(&Key::from("cyclePHIDs"))
        .map(as_list)
        .unwrap_or_default()
}

/// `HeraldRuleResult::newDetailsView()`.
fn rule_details(result: &ResultMap, handles: &HashMap<Phid, Handle>) -> Option<String> {
    match result.code.as_str() {
        "exception" => {
            let mut class = data_string(result, "exception.class");
            let mut message = data_string(result, "exception.message");
            if class.is_empty() {
                "Unknown Error".clone_into(&mut class);
            }
            if message.is_empty() {
                UNKNOWN_ERROR.clone_into(&mut message);
            }
            Some(error_html(&class, &message))
        }
        "recursion" => {
            let links: Vec<String> = cycle_phids(result)
                .iter()
                .map(|phid| harbormaster::handle(handles, &Phid::new(phid.as_str())).link_html())
                .collect();
            Some(format!(
                "This rule has a dependency cycle and can not be evaluated: {}",
                links.join(" &gt; ")
            ))
        }
        _ => None,
    }
}

/// `renderConditionTestValue()`: values naming objects show their
/// handle's name, sorted.
fn test_value_html(field: &str, value: &Php, handles: &HashMap<Phid, Handle>) -> String {
    let value = if field == "rule" {
        Php::list([value.clone()])
    } else {
        value.clone()
    };
    let shown = match &value {
        Php::Array(_) | Php::Object { .. } => {
            let mut names: Vec<String> = as_list(&value)
                .into_iter()
                .map(|value| {
                    match handles.get(&Phid::new(value.as_str())) {
                        Some(handle) if handle.uri.is_some() => handle.name.clone(),
                        // Restricted objects, task priorities, statuses...
                        _ => value,
                    }
                })
                .collect();
            names.sort();
            names.join(", ")
        }
        other => text(other),
    };
    format!(
        "<span class=\"condition-test-value\">{}</span>",
        escape(&shown)
    )
}

/// `getTranscriptPHIDs()`, and the PHIDs the effects and results name.
fn transcript_phids(transcript: &Transcript) -> Vec<Phid> {
    let mut values: Vec<String> = Vec::new();
    if let Some(object) = &transcript.object {
        values.push(object.phid.clone());
        for (key, value) in &object.fields {
            values.extend(
                transcript_value_phids(key, value)
                    .into_iter()
                    .map(|phid| phid.as_str().to_owned()),
            );
        }
        for frame in object.profile.iter().flatten() {
            if frame.kind == "rule" {
                values.push(frame.key.clone());
            }
        }
    }
    for applied in &transcript.applied {
        values.extend(as_list(&applied.target));
        if let Php::Array(entries) = &applied.applied_reason {
            for (_, entry) in entries {
                if let Some(data) = entry.get(&Key::from("data")) {
                    values.extend(
                        effect_phids(data)
                            .into_iter()
                            .map(|phid| phid.as_str().to_owned()),
                    );
                }
            }
        }
    }
    for rule in &transcript.rules {
        values.push(rule.owner.clone());
        values.extend(cycle_phids(&rule.result));
    }
    for (_, conditions) in &transcript.conditions {
        for condition in conditions {
            if condition.field == "rule" {
                values.push(text(&condition.test_value));
            } else if let Php::Array(_) = condition.test_value {
                values.extend(as_list(&condition.test_value));
            }
        }
    }
    let mut phids = phids_of(&values);
    phids.sort();
    phids.dedup();
    phids
}

/// The rule transcript (`buildActionTranscriptPanel()`).
fn rules_view(
    transcript: &Transcript,
    handles: &HashMap<Phid, Handle>,
) -> Result<Vec<RuleBox>, PageError> {
    let mut rules: Vec<_> = transcript.rules.iter().collect();
    rules.sort_by_key(|rule| rule.rule_id.parse::<i64>().unwrap_or_default());
    let mut boxes = Vec::new();
    for rule in rules {
        let (icon, color, name, description, applied) = rule_result(&rule.result.code)?;
        let mut conditions = Vec::new();
        for condition in transcript.conditions_of(&rule.rule_id) {
            let (icon, color, name) = condition_result(&condition.result.code)?;
            let explanation = format!(
                "{} {} {}",
                escape(field_name(&condition.field)?),
                escape(condition_name(&condition.condition).unwrap_or("Unknown")),
                test_value_html(&condition.field, &condition.test_value, handles)
            );
            let details = condition_details(&condition.result)?
                .map(|details| format!("<div class=\"herald-condition-note\">{details}</div>"))
                .unwrap_or_default();
            conditions.push(StatusItem {
                icon: Some(icon),
                color: Some(color),
                target_html: escape(name),
                note_html: format!("{explanation}{details}"),
            });
        }
        let details = rule_details(&rule.result, handles)
            .map(|details| format!("<div class=\"herald-condition-note\">{details}</div>"))
            .unwrap_or_default();
        let result = StatusItem {
            icon: Some(icon),
            color: Some(color),
            target_html: format!("<strong>{}</strong>", escape(name)),
            note_html: format!("{}{details}", escape(description)),
        };
        let mut actions = Vec::new();
        for applied in transcript
            .applied
            .iter()
            .filter(|applied| applied.rule_id == rule.rule_id)
        {
            let name = format!("Action: {}", super::action_name(&applied.action)?);
            let mut items = Vec::new();
            if transcript.row.dry_run {
                items.push(StatusItem {
                    icon: Some("fa-ban"),
                    color: Some("grey"),
                    target_html: "Dry Run".to_owned(),
                    note_html: "This was a dry run, so no actions were taken.".to_owned(),
                });
            } else if let Php::Array(entries) = &applied.applied_reason {
                for (_, entry) in entries {
                    let kind = entry.get(&Key::from("type")).map(text).unwrap_or_default();
                    let data = entry.get(&Key::from("data")).cloned().unwrap_or(Php::Null);
                    let view = effect(&applied.action, &kind, &data, handles)?;
                    items.push(StatusItem {
                        icon: view.icon,
                        color: view.color,
                        target_html: view.name.map(escape).unwrap_or_default(),
                        note_html: view.description_html.unwrap_or_default(),
                    });
                }
            } else {
                items.push(StatusItem {
                    icon: Some("fa-clock-o"),
                    color: Some("grey"),
                    target_html: "Old Transcript".to_owned(),
                    note_html: "This is an old transcript which uses an obsolete log format. \
                                Detailed action information is not available."
                        .to_owned(),
                });
            }
            actions.push(ActionBox { name, items });
        }
        boxes.push(RuleBox {
            id: rule.rule_id.clone(),
            name: rule.name.clone(),
            applied,
            conditions,
            result,
            actions,
        });
    }
    Ok(boxes)
}

/// The object transcript (`buildObjectTranscriptPanel()`).
fn fields_view(
    transcript: &Transcript,
    handles: &HashMap<Phid, Handle>,
) -> Result<Vec<(String, String)>, PageError> {
    let Some(object) = &transcript.object else {
        return Ok(Vec::new());
    };
    let row = &transcript.row;
    #[allow(clippy::cast_possible_truncation)]
    let duration = (1000.0 * row.duration) as i64;
    let mut rows = vec![
        ("Object Name".to_owned(), escape(&object.name)),
        ("Object Type".to_owned(), escape(&object.content_type)),
        ("Object PHID".to_owned(), escape(&object.phid)),
        (
            "Object Link".to_owned(),
            harbormaster::handle(handles, &Phid::new(object.phid.as_str())).link_html(),
        ),
        (
            "Run At Epoch".to_owned(),
            escape(&crate::time::format_long_utc(row.time)),
        ),
        ("Run On Host".to_owned(), escape(&row.host)),
        ("Run Duration".to_owned(), format!("{duration} ms")),
    ];
    for (key, value) in &object.fields {
        rows.push((
            format!("Field: {}", field_name(key)?),
            transcript_value_html(key, value, handles)?,
        ));
    }
    Ok(rows)
}

/// The profile (`buildProfilerTranscriptPanel()`): the costliest first.
fn profile_view(
    transcript: &Transcript,
    handles: &HashMap<Phid, Handle>,
) -> Result<Option<Vec<ProfileRow>>, PageError> {
    let Some(frames) = transcript
        .object
        .as_ref()
        .and_then(|object| object.profile.as_ref())
    else {
        return Ok(None);
    };
    // `isort()` then `array_reverse()`.
    let mut frames: Vec<_> = frames.iter().collect();
    frames.sort_by(|left, right| left.elapsed.total_cmp(&right.elapsed));
    frames.reverse();
    frames
        .into_iter()
        .map(|frame| {
            let (kind, what_html) = match frame.kind.as_str() {
                "rule" => (
                    "Rule".to_owned(),
                    harbormaster::handle(handles, &Phid::new(frame.key.as_str())).link_html(),
                ),
                "field" => ("Field".to_owned(), escape(field_name(&frame.key)?)),
                other => (other.to_owned(), escape(&frame.key)),
            };
            #[allow(clippy::cast_possible_truncation)]
            let cost = (1_000_000.0 * frame.elapsed).round() as i64;
            Ok(ProfileRow {
                kind,
                what_html,
                cost: format!("{}us", super::lists::thousands(cost)),
                count: super::lists::thousands(frame.count),
            })
        })
        .collect::<Result<Vec<_>, PageError>>()
        .map(Some)
}

async fn render<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    id: &str,
    view: Option<&str>,
    uri: &Uri,
) -> Result<Page<TranscriptPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let state = state.as_ref();
    let id: i64 =
        (!id.is_empty() && !id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()))
            .then(|| id.parse().ok())
            .flatten()
            .ok_or(PageError::NotFound)?;
    let transcript = herald::load_transcript(state.database(), id)
        .await
        .map_err(harbormaster::internal)?
        .ok_or(PageError::NotFound)?;
    if transcript.row.object_phid.is_empty() {
        return Err(PageError::Fallback("a transcript without object"));
    }
    let scope = Scope::new(state, &viewer);
    let object = Phid::new(transcript.row.object_phid.as_str());
    let object_handle = load_handles(state, &scope, std::slice::from_ref(&object)).await?;
    // The object query rejects the transcript, which raises a policy
    // exception.
    if harbormaster::handle(&object_handle, &object).uri.is_none() {
        return Err(PageError::PolicyDenied);
    }
    let view: &'static str = match view {
        None => "rules",
        Some("fields") => "fields",
        Some("profile") => "profile",
        Some("xactions") => return Err(PageError::Fallback("the transactions of a transcript")),
        Some(_) => return Err(PageError::NotFound),
    };
    let has_transactions = transcript
        .object
        .as_ref()
        .and_then(|object| object.applied_transaction_phids.as_ref())
        .is_some_and(|phids| !phids.is_empty());
    let content = match &transcript.object {
        None => Content::Old,
        Some(object) => {
            // PHP refuses transcripts of adapters the viewer can't use.
            let rule = herald::RuleRow {
                id: 0,
                phid: Phid::new("PHID-HRUL-transcript"),
                name: String::new(),
                author_phid: viewer.user.phid.clone(),
                content_type: object.content_type.clone(),
                must_match_all: false,
                repetition_policy: String::new(),
                rule_type: "global".to_owned(),
                is_disabled: false,
                trigger_object_phid: None,
            };
            if super::rule::visible_rules(state, &scope, vec![rule])
                .await?
                .is_empty()
            {
                return Err(PageError::Fallback(
                    "a transcript of an adapter the viewer can't use",
                ));
            }
            if object.content_type != REVISIONS {
                return Err(PageError::Fallback("a transcript of an adapter not ported"));
            }
            let handles = load_handles(state, &scope, &transcript_phids(&transcript)).await?;
            match view {
                "fields" => Content::Fields(fields_view(&transcript, &handles)?),
                "profile" => Content::Profile(profile_view(&transcript, &handles)?),
                _ => Content::Rules(rules_view(&transcript, &handles)?),
            }
        }
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(TranscriptPage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("Herald Transcript {id}"),
            current,
        ),
        id,
        dry_run: transcript.row.dry_run,
        view,
        has_transactions,
        content,
    }))
}

/// `GET /herald/transcript/{id}/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<TranscriptPage>, PageError> {
    render(state, viewer, &id, None, &uri).await
}

/// `GET /herald/transcript/{id}/{view}/`.
pub(crate) async fn show_view<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((id, view)): Path<(String, String)>,
    uri: Uri,
) -> Result<Page<TranscriptPage>, PageError> {
    render(state, viewer, &id, Some(&view), &uri).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_values_show_names_of_visible_objects() {
        let mut handles = HashMap::new();
        handles.insert(
            Phid::new("PHID-USER-b"),
            Handle {
                name: "zed".to_owned(),
                full_name: "zed".to_owned(),
                uri: Some("/p/zed/".to_owned()),
                closed: false,
            },
        );
        handles.insert(
            Phid::new("PHID-PROJ-x"),
            Handle {
                name: "Restricted Project".to_owned(),
                full_name: "Restricted Project".to_owned(),
                uri: None,
                closed: false,
            },
        );
        let value = Php::list([Php::str("PHID-USER-b"), Php::str("PHID-PROJ-x")]);
        assert_eq!(
            test_value_html("cc", &value, &handles),
            "<span class=\"condition-test-value\">PHID-PROJ-x, zed</span>"
        );
        assert_eq!(
            test_value_html("differential.revision.title", &Php::str("a<b"), &handles),
            "<span class=\"condition-test-value\">a&lt;b</span>"
        );
    }
}
