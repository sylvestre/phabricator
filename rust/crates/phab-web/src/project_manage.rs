//! A project's "Manage" page (`/project/manage/<id>/`,
//! `PhabricatorProjectManageController`), read-only: its details, its
//! timeline and the curtain's actions, which link to PHP's workflows.
//!
//! The timeline shows the titles of `PhabricatorProjectTransaction`
//! (`getTitle()`) for the name, icon, color, hashtags, status, membership
//! lock, members, watchers, policies and description. Left to PHP:
//! timelines with comments, other transactions (images, parents,
//! milestones, custom fields...), policies which are neither global nor a
//! project or a user, more transactions than a page and installs with
//! configured project fields, whose view PHP builds from their classes.

use std::collections::HashMap;
use std::num::FpCategory;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_dashboard::storage::{self as dashboard, Transaction};
use phab_app_differential::storage as differential;
use phab_app_project::{Project, policy, storage};
use phab_core::{Phid, PhidType, php};
use phab_policy::{Capability, application};
use phab_remarkup::{Markup, Mode};
use serde_json::Value;

use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Action, Handle, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;
use crate::{remarkup, time};

/// The description's custom field key.
const DESCRIPTION: &str = "std:project:internal:description";
/// `PhabricatorProjectSilencedEdgeType`.
const SILENCED: i64 = 61;
/// `PhabricatorObjectHasWatcherEdgeType`.
const WATCHER: i64 = 47;
/// `PhabricatorMutedEdgeType`, `PhabricatorObjectMentionsObjectEdgeType`
/// and `PhabricatorMutedByEdgeType`, which every timeline hides.
const HIDDEN_EDGES: [i64; 3] = [52, 67, 68];
/// The timeline pages its transactions.
const TIMELINE_LIMIT: usize = 100;

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

#[derive(Template)]
#[template(path = "project_manage.html")]
pub(crate) struct ManagePage {
    shell: Shell,
    id: i64,
    name: String,
    uri: String,
    icon: String,
    color: String,
    archived: bool,
    hashtags: String,
    review_queue: usize,
    review_queue_uri: String,
    description: Option<Markup>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

impl ManagePage {
    fn description_html(&self) -> &str {
        self.description.as_ref().map_or("", |description| {
            std::str::from_utf8(description.as_bytes()).unwrap_or("")
        })
    }
}

/// A quoted value of a title (`renderValue()`).
fn quoted(text: &str) -> String {
    format!("\"{}\"", escape(text))
}

/// The PHIDs of an edge list, old (`[phid, ...]`) or not (`{phid: edge}`).
fn edge_phids(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Value::Object(map) => Some(map.keys().cloned().collect()),
        _ => None,
    }
}

/// A list of strings, as `array_diff()` reads it.
fn strings(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Value::Object(map) => map
            .values()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

fn minus(from: &[String], other: &[String]) -> Vec<String> {
    from.iter()
        .filter(|item| !other.contains(item))
        .cloned()
        .collect()
}

fn edge_type(transaction: &Transaction) -> Option<i64> {
    let int = |value: &Value| match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    };
    // `head()` of the metadata value, which old rows store as a list.
    match transaction.metadata.get("edge:type") {
        Some(Value::Array(items)) => items.first().and_then(int),
        Some(value) => int(value),
        None => None,
    }
}

fn is_policy(kind: &str) -> bool {
    matches!(
        kind,
        "core:view-policy" | "core:edit-policy" | "core:join-policy"
    )
}

/// `PhabricatorApplicationTransaction::shouldHide()` with the project's
/// silenced members edge.
fn should_hide(transaction: &Transaction) -> bool {
    let kind = transaction.transaction_type.as_str();
    let is_create = transaction.metadata_flag("core.create");
    if is_create {
        match kind {
            "core:create" | "core:space" => {}
            _ if is_policy(kind) => {}
            "core:subtype" => return true,
            _ => {
                let empty = match &transaction.old_value {
                    Value::Null => true,
                    Value::String(text) => text.is_empty(),
                    Value::Array(items) => items.is_empty(),
                    Value::Object(map) => map.is_empty(),
                    Value::Number(number) => number.as_i64() == Some(0),
                    Value::Bool(_) => false,
                };
                if empty {
                    return true;
                }
            }
        }
        if transaction.metadata_flag("core.default") {
            return true;
        }
    }
    if is_policy(kind) && !is_create && transaction.old_value.is_null() {
        return true;
    }
    kind == "core:edge"
        && edge_type(transaction)
            .is_some_and(|edge| edge == SILENCED || HIDDEN_EDGES.contains(&edge))
}

/// `PhabricatorPolicyRef::getPolicyDisplayName()`: the policy's
/// `getFullName()`.
fn policy_name(value: &Value, handles: &HashMap<Phid, Handle>) -> Result<String, PageError> {
    let Some(policy) = value.as_str() else {
        return Err(PageError::Fallback("an unusual project policy"));
    };
    Ok(match policy {
        "public" => "Public (No Login Required)".to_owned(),
        "users" => "All Users".to_owned(),
        "admin" => "Administrators".to_owned(),
        "no-one" => "No One".to_owned(),
        "obj.project.members" => "Project Members".to_owned(),
        _ => {
            let phid = Phid::new(policy);
            match phid.phid_type().as_str() {
                "PROJ" => format!(
                    "Members of Project: {}",
                    harbormaster::handle(handles, &phid).name
                ),
                "USER" => format!(
                    "Only User: {}",
                    harbormaster::handle(handles, &phid).full_name
                ),
                _ => return Err(PageError::Fallback("custom policies in a project timeline")),
            }
        }
    })
}

/// `PHUITagView::getShadeName()`.
fn shade_name(shade: &str) -> &str {
    match shade {
        "red" => "Red",
        "orange" => "Orange",
        "yellow" => "Yellow",
        "blue" => "Blue",
        "indigo" => "Indigo",
        "violet" => "Violet",
        "green" => "Green",
        "grey" => "Grey",
        "pink" => "Pink",
        "checkered" => "Checkered",
        "disabled" => "Disabled",
        other => other,
    }
}

/// A list of handles (`renderHandleList()`).
fn handle_list(phids: &[String], handles: &HashMap<Phid, Handle>) -> String {
    phids
        .iter()
        .map(|phid| harbormaster::handle(handles, &Phid::new(phid.as_str())).short_link_html())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The PHIDs a title names besides its author.
fn named_phids(transaction: &Transaction) -> Vec<Phid> {
    let kind = transaction.transaction_type.as_str();
    let mut phids = Vec::new();
    if kind == "core:edge" {
        for value in [&transaction.old_value, &transaction.new_value] {
            phids.extend(edge_phids(value).into_iter().flatten().map(Phid::new));
        }
    } else if is_policy(kind) {
        for value in [&transaction.old_value, &transaction.new_value] {
            if let Some(policy) = value.as_str().filter(|policy| policy.starts_with("PHID-")) {
                phids.push(Phid::new(policy));
            }
        }
    }
    phids
}

/// `$old == 0` of a status transaction: the project was active.
fn was_active(old: &Value) -> Result<bool, PageError> {
    Ok(match old {
        Value::Null | Value::Bool(false) => true,
        Value::Bool(true) => false,
        Value::Number(number) => number
            .as_f64()
            .is_some_and(|number| number.classify() == FpCategory::Zero),
        Value::String(text) => match text.trim().parse::<f64>() {
            Ok(number) => number.classify() == FpCategory::Zero,
            Err(_) => return Err(PageError::Fallback("an unusual project transaction")),
        },
        _ => return Err(PageError::Fallback("an unusual project transaction")),
    })
}

/// The title of a list change: `(edited, noun)` name both changes and
/// one, `added` and `removed` are the counts and the rendered lists.
fn list_title(
    author: &str,
    (edited, noun): (&str, &str),
    (added, added_html): (usize, String),
    (removed, removed_html): (usize, String),
) -> Result<String, PageError> {
    Ok(match (added, removed) {
        (0, 0) => return Err(PageError::Fallback("an empty list transaction")),
        (_, 0) => format!("{author} added {added} {noun}: {added_html}."),
        (0, _) => format!("{author} removed {removed} {noun}: {removed_html}."),
        _ => format!(
            "{author} {edited}, added {added}: {added_html}; removed {removed}: {removed_html}."
        ),
    })
}

/// `getTitle()` of a project transaction which isn't hidden.
fn title(
    transaction: &Transaction,
    config: &phab_core::Config,
    handles: &HashMap<Phid, Handle>,
) -> Result<String, PageError> {
    let author = harbormaster::handle(handles, &transaction.author_phid).short_link_html();
    let (old, new) = (&transaction.old_value, &transaction.new_value);
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or(PageError::Fallback("an unusual project transaction"))
    };
    let is_create = transaction.metadata_flag("core.create");
    Ok(match transaction.transaction_type.as_str() {
        "core:create" => format!("{author} created this project."),
        "project:name" if old.is_null() => format!("{author} created this project."),
        "project:name" => format!(
            "{author} renamed this project from {} to {}.",
            quoted(&text(old)?),
            quoted(&text(new)?)
        ),
        "project:icon" => format!(
            "{author} set this project's icon to {}.",
            quoted(&phab_app_project::web::icon_label(config, &text(new)?))
        ),
        "project:color" => format!(
            "{author} set this project's color to {}.",
            quoted(shade_name(&text(new)?))
        ),
        "project:slugs" => {
            let (Some(old), Some(new)) = (strings(old), strings(new)) else {
                return Err(PageError::Fallback("an unusual project transaction"));
            };
            let list = |tags: &[String]| {
                tags.iter()
                    .map(|tag| quoted(&format!("#{tag}")))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (added, removed) = (minus(&new, &old), minus(&old, &new));
            list_title(
                &author,
                ("changed project hashtag(s)", "project hashtag(s)"),
                (added.len(), list(&added)),
                (removed.len(), list(&removed)),
            )?
        }
        "project:status" if was_active(old)? => format!("{author} archived this project."),
        "project:status" => format!("{author} activated this project."),
        "project:locked" if php::is_truthy(new) => {
            format!("{author} locked this project's membership.")
        }
        "project:locked" => format!("{author} unlocked this project's membership."),
        kind if is_policy(kind) => {
            let what = match kind {
                "core:view-policy" => "visibility",
                "core:edit-policy" => "edit policy",
                _ => "join policy",
            };
            if is_create {
                format!(
                    "{author} created this object with {what} \"{}\".",
                    escape(&policy_name(new, handles)?)
                )
            } else {
                format!(
                    "{author} changed the {what} from \"{}\" to \"{}\".",
                    escape(&policy_name(old, handles)?),
                    escape(&policy_name(new, handles)?)
                )
            }
        }
        "core:customfield"
            if transaction
                .metadata
                .get("customfield:key")
                .and_then(Value::as_str)
                == Some(DESCRIPTION) =>
        {
            format!("{author} edited Description.")
        }
        "core:edge" => {
            let (Some(old), Some(new)) = (edge_phids(old), edge_phids(new)) else {
                return Err(PageError::Fallback("an unusual project transaction"));
            };
            let nouns = match edge_type(transaction) {
                Some(phab_db::edges::PROJECT_HAS_MEMBER) => ("edited member(s)", "member(s)"),
                Some(WATCHER) => ("edited watcher(s)", "watcher(s)"),
                _ => return Err(PageError::Fallback("a project edge not ported")),
            };
            let (added, removed) = (minus(&new, &old), minus(&old, &new));
            list_title(
                &author,
                nouns,
                (added.len(), handle_list(&added, handles)),
                (removed.len(), handle_list(&removed, handles)),
            )?
        }
        _ => return Err(PageError::Fallback("a project transaction not ported")),
    })
}

/// The timeline of the project: the transactions the viewer may see.
async fn timeline<S: WebState>(
    state: &S,
    viewer: &WebUser,
    project: &Project,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let transactions =
        dashboard::object_transactions(database, ("project", "project_transaction"), &project.phid)
            .await?;
    let subjects: Vec<_> = transactions.iter().map(Transaction::subject).collect();
    let visible = differential::visible_subjects(
        database,
        state.config(),
        &viewer.user.viewer(),
        &subjects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let shown: Vec<&Transaction> = transactions
        .iter()
        .zip(visible)
        .filter_map(|(transaction, visible)| visible.then_some(transaction))
        .collect();
    if shown.len() > TIMELINE_LIMIT {
        return Err(PageError::Fallback("a long project timeline"));
    }
    if shown
        .iter()
        .any(|transaction| transaction.comment_phid.is_some())
    {
        return Err(PageError::Fallback("a comment in a project timeline"));
    }
    let shown: Vec<&Transaction> = shown
        .into_iter()
        .filter(|transaction| !should_hide(transaction))
        .collect();
    if shown
        .iter()
        .any(|transaction| transaction.author_phid.phid_type() != PhidType::USER)
    {
        return Err(PageError::Fallback("a transaction by an application"));
    }
    let mut phids: Vec<Phid> = Vec::new();
    for transaction in &shown {
        phids.push(transaction.author_phid.clone());
        phids.extend(named_phids(transaction));
    }
    phids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    phids.dedup();
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &phids).await?;
    shown
        .into_iter()
        .map(|transaction| {
            Ok(Entry {
                title_html: title(transaction, state.config(), &handles)?,
                date: time::format_utc(transaction.date_created),
                comment_html: None,
            })
        })
        .collect()
}

/// The curtain's actions (`buildCurtain()`), which link to PHP.
fn actions(id: i64, archived: bool, can_edit: bool) -> Vec<Action> {
    let action = |name, icon, uri: String| Action {
        name,
        icon,
        uri,
        enabled: can_edit,
    };
    vec![
        action("Edit Details", "fa-pencil", format!("/project/edit/{id}/")),
        action(
            "Edit Menu",
            "fa-th-list",
            format!("/project/{id}/item/configure/"),
        ),
        action(
            "Edit Picture",
            "fa-picture-o",
            format!("/project/picture/{id}/"),
        ),
        if archived {
            action(
                "Activate Project",
                "fa-check",
                format!("/project/archive/{id}/"),
            )
        } else {
            action(
                "Archive Project",
                "fa-ban",
                format!("/project/archive/{id}/"),
            )
        },
    ]
}

/// `GET /project/manage/{id}/`.
pub(crate) async fn manage<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<ManagePage>, PageError> {
    viewer
        .require_application(&*state, application::PROJECT)
        .await?;
    let state = state.as_ref();
    let config = state.config();
    if config.value("projects.fields").is_some()
        || config
            .value("projects.custom-field-definitions")
            .is_some_and(php::is_truthy)
    {
        return Err(PageError::Fallback("project custom fields"));
    }
    let database = state.database();
    let project = storage::load_by_id(database, id)
        .await?
        .ok_or(PageError::NotFound)?;
    let as_viewer = viewer.user.viewer();
    let (mut visible, graph) =
        policy::visible_projects(database, config, &as_viewer, vec![project], state.now())
            .await
            .map_err(internal)?;
    // The project exists: PHP's executeOne() raises a policy exception.
    let project = visible.pop().ok_or(PageError::PolicyDenied)?;
    let can_edit = policy::has_capability(
        database,
        config,
        &as_viewer,
        &project,
        Capability::EDIT,
        state.now(),
    )
    .await
    .map_err(internal)?;

    let slugs = phab_app_project::web::slugs(database, &project).await?;
    let hashtags = slugs
        .iter()
        .map(|slug| format!("#{slug}"))
        .collect::<Vec<_>>()
        .join(", ");
    let reviewed = differential::load_open_revisions_reviewed_by(database, &project.phid).await?;
    let reviewed =
        differential::visible_revisions(database, config, &as_viewer, reviewed, state.now())
            .await
            .map_err(internal)?;
    let review_queue = crate::people::review_queue_length(&reviewed, &project.phid);
    let review_queue_uri = format!(
        "/differential/?responsiblePHIDs%5B0%5D={}&statuses%5B0%5D=open()&order=newest&bucket=action",
        project.phid.as_str()
    );
    let description = storage::load_description(database, &project.phid).await?;
    let description = if description.is_empty() {
        None
    } else {
        Some(remarkup::render(state, &as_viewer, &description, Mode::Html).await?)
    };
    let timeline = timeline(state, &viewer, &project).await?;
    let name = project.display_name(graph.parent(&project));
    Ok(Page(ManagePage {
        shell: Shell::for_user(config, &viewer, &format!("{name} · Manage"), current(&uri)),
        id: project.id,
        uri: project.uri(),
        icon: project.icon.clone(),
        color: project.color.clone(),
        archived: project.is_archived(),
        hashtags,
        review_queue,
        review_queue_uri,
        description,
        timeline,
        actions: actions(project.id, project.is_archived(), can_edit),
        name,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn transaction(kind: &str, old: Value, new: Value, metadata: Value) -> Transaction {
        let Value::Object(metadata) = metadata else {
            panic!("metadata is an object");
        };
        Transaction {
            phid: Phid::new("PHID-XACT-PROJ-1"),
            author_phid: Phid::new("PHID-USER-alice"),
            view_policy: "public".to_owned(),
            edit_policy: "PHID-USER-alice".to_owned(),
            comment_phid: None,
            transaction_type: kind.to_owned(),
            old_value: old,
            new_value: new,
            metadata,
            date_created: 1,
        }
    }

    #[test]
    fn hides_like_php() {
        let create = json!({"core.create": true});
        assert!(should_hide(&transaction(
            "project:name",
            Value::Null,
            json!("a"),
            create.clone()
        )));
        assert!(!should_hide(&transaction(
            "project:icon",
            json!("project"),
            json!("group"),
            create.clone()
        )));
        assert!(!should_hide(&transaction(
            "core:view-policy",
            Value::Null,
            json!("users"),
            create.clone()
        )));
        assert!(should_hide(&transaction(
            "core:view-policy",
            Value::Null,
            json!("users"),
            json!({"core.create": true, "core.default": true})
        )));
        assert!(should_hide(&transaction(
            "core:edge",
            json!([]),
            json!(["PHID-USER-alice"]),
            json!({"edge:type": 61})
        )));
        assert!(!should_hide(&transaction(
            "core:edge",
            json!([]),
            json!(["PHID-USER-alice"]),
            json!({"edge:type": 13})
        )));
    }

    #[test]
    fn titles_edges_old_and_new() {
        let handles = HashMap::new();
        let config = phab_core::Config::default();
        let joined = transaction(
            "core:edge",
            json!({"PHID-USER-bob": {"dst": "PHID-USER-bob"}}),
            json!({"PHID-USER-bob": {}, "PHID-USER-carol": {}}),
            json!({"edge:type": 13}),
        );
        let title = title(&joined, &config, &handles).unwrap();
        assert!(
            title.ends_with(" added 1 member(s): Unknown Object."),
            "{title}"
        );
        let status = transaction("project:status", json!("0"), json!("100"), json!({}));
        assert!(
            super::title(&status, &config, &handles)
                .unwrap()
                .ends_with(" archived this project.")
        );
        let slugs = transaction(
            "project:slugs",
            json!(["a"]),
            json!(["b"]),
            Value::Object(Map::new()),
        );
        assert!(
            super::title(&slugs, &config, &handles)
                .unwrap()
                .ends_with(" changed project hashtag(s), added 1: \"#b\"; removed 1: \"#a\".")
        );
    }
}
