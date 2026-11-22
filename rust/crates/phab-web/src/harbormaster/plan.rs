//! `/harbormaster/plan/<id>/`: a build plan, its steps, behaviors, the
//! Herald rules running it and its recent builds
//! (`HarbormasterPlanViewController`).

use std::collections::HashSet;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::{self as harbormaster, Lookup, Plan, Step};
use phab_core::{Phid, php};
use phab_policy::application;
use serde_json::Value;

use super::status::{self, Status};
use super::{Action, Scope, TransactionTable, method_error};
use crate::diff_html;
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;

/// How many builds and rules the page lists.
const LIST_LIMIT: usize = 10;

/// A behavior option (`HarbormasterBuildPlanBehaviorOption`): key, name,
/// icon and description.
type BehaviorOption = (&'static str, &'static str, &'static str, &'static str);

/// `HarbormasterBuildPlanBehavior::newPlanBehaviors()`: key, name and
/// options, the default first.
const BEHAVIORS: [(&str, &str, &[BehaviorOption]); 5] = [
    (
        "hold-drafts",
        "Hold Drafts",
        &[
            (
                "always",
                "Always",
                "fa-check-circle-o green",
                "Revisions are not sent for review until the build completes, and are returned \
                 to the author for updates if the build fails.",
            ),
            (
                "building",
                "If Building",
                "fa-pause-circle-o yellow",
                "Revisions are not sent for review until the build completes, but they will be \
                 sent for review even if it fails.",
            ),
            (
                "never",
                "Never",
                "fa-circle-o red",
                "Revisions are sent for review regardless of the status of the build.",
            ),
        ],
    ),
    (
        "arc-land",
        "Warn When Landing",
        &[
            (
                "always",
                "Always",
                "fa-check-circle-o green",
                "\"arc land\" warns if the build is still running or has failed.",
            ),
            (
                "building",
                "If Building",
                "fa-pause-circle-o yellow",
                "\"arc land\" warns if the build is still running, but ignores the build if it \
                 has failed.",
            ),
            (
                "complete",
                "If Complete",
                "fa-dot-circle-o yellow",
                "\"arc land\" warns if the build has failed, but ignores the build if it is \
                 still running.",
            ),
            (
                "never",
                "Never",
                "fa-circle-o red",
                "\"arc land\" never warns that the build is still running or has failed.",
            ),
        ],
    ),
    (
        "buildable",
        "Affects Buildable",
        &[
            (
                "always",
                "Always",
                "fa-check-circle-o green",
                "The buildable waits for the build, and fails if the build fails.",
            ),
            (
                "building",
                "If Building",
                "fa-pause-circle-o yellow",
                "The buildable waits for the build, but does not fail if the build fails.",
            ),
            (
                "never",
                "Never",
                "fa-circle-o red",
                "The buildable does not wait for the build.",
            ),
        ],
    ),
    (
        "restartable",
        "Restartable",
        &[
            (
                "always",
                "Always",
                "fa-repeat green",
                "The build may be restarted.",
            ),
            (
                "failed",
                "If Failed",
                "fa-times-circle-o yellow",
                "The build may be restarted if it has failed.",
            ),
            (
                "never",
                "Never",
                "fa-times red",
                "The build may not be restarted.",
            ),
        ],
    ),
    (
        "runnable",
        "Runnable",
        &[
            (
                "edit",
                "If Editable",
                "fa-pencil green",
                "Only users who can edit the plan can run it manually.",
            ),
            (
                "view",
                "If Viewable",
                "fa-exclamation-triangle yellow",
                "Any user who can view the plan can run it manually.",
            ),
        ],
    ),
];

/// `HarbormasterBuildPlanBehavior::getPlanOption()`: the option stored in
/// the plan's `behavior.<key>` property if it is one, else the default.
fn plan_option(plan: &Plan, behavior: &str) -> BehaviorOption {
    let options = BEHAVIORS
        .iter()
        .find(|(key, _, _)| *key == behavior)
        .map_or(&[][..], |(_, _, options)| *options);
    let stored = plan.properties.get(&format!("behavior.{behavior}"));
    options
        .iter()
        .find(|(key, ..)| stored.and_then(Value::as_str) == Some(*key))
        .or_else(|| options.first())
        .copied()
        .unwrap_or(("", "", "", ""))
}

/// The key of the plan's option for a behavior.
pub(crate) fn behavior_option(plan: &Plan, behavior: &str) -> &'static str {
    plan_option(plan, behavior).0
}

/// `HarbormasterBuildPlan::canRunWithoutEditCapability()`.
pub(crate) fn can_run_without_edit_capability(plan: &Plan) -> bool {
    behavior_option(plan, "runnable") == "view"
}

/// A step in the list.
pub(crate) struct StepItem {
    id: i64,
    label: String,
    name: String,
    description_html: String,
    /// The steps it depends on, by name; `None` for missing ones.
    depends: Vec<Option<String>>,
    /// The red warning icon: an unknown implementation, or a deadlock.
    warning: bool,
}

/// A row of the behaviors table.
pub(crate) struct BehaviorRow {
    key: &'static str,
    icon: &'static str,
    color: &'static str,
    name: &'static str,
    option: &'static str,
    description: &'static str,
}

/// A recent build.
pub(crate) struct BuildItem {
    id: i64,
    name: String,
    object: String,
    initiator_html: Option<String>,
    date: String,
    status: Status,
}

/// A Herald rule running the plan.
pub(crate) struct RuleItem {
    id: i64,
    name: String,
    kind: &'static str,
    icon: &'static str,
    author_html: Option<String>,
    author_inactive: bool,
    affects: &'static str,
}

#[derive(Template)]
#[template(path = "plan.html")]
pub(crate) struct PlanPage {
    shell: Shell,
    id: i64,
    plan_phid: String,
    name: String,
    disabled: bool,
    error: Option<&'static str>,
    can_edit: bool,
    steps: Vec<StepItem>,
    behaviors: Vec<BehaviorRow>,
    rules: Vec<RuleItem>,
    more_rules: bool,
    builds: Vec<BuildItem>,
    more_builds: bool,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

impl PlanPage {
    fn builds_uri(&self) -> String {
        format!("/harbormaster/build/?plan={}", self.plan_phid)
    }

    fn rules_uri(&self) -> String {
        format!("/herald/?affectedPHID={}", self.plan_phid)
    }
}

/// `formatValueForDescription()`.
fn description_value(value: &str) -> String {
    if value.is_empty() {
        "<em>(null)</em>".to_owned()
    } else {
        format!("<strong>{}</strong>", diff_html::escape(value))
    }
}

/// `PhutilURI::getDomain()`, for the URIs PHP and Rust read alike.
fn domain(uri: &str) -> Result<String, PageError> {
    let Some((_, rest)) = uri.split_once("://") else {
        return Err(PageError::Fallback("an unusual HTTP step URI"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.starts_with('[') {
        return Err(PageError::Fallback("an IPv6 HTTP step URI"));
    }
    Ok(host.split(':').next().unwrap_or("").to_owned())
}

/// A setting of a step, as `getSetting()` gives a string.
fn setting<'a>(step: &'a Step, key: &str) -> Result<Option<&'a str>, PageError> {
    match step.details.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(PageError::Fallback("a step setting which is not a string")),
    }
}

/// `HarbormasterBuildStepImplementation::getDescription()`, as HTML.
fn step_description(step: &Step) -> Result<String, PageError> {
    Ok(match step.class_name.as_str() {
        "HarbormasterHTTPRequestBuildStepImplementation" => {
            let domain = match setting(step, "uri")? {
                Some(uri) if php::is_truthy_str(uri) => domain(uri)?,
                _ => String::new(),
            };
            // `idx()` gives the default only for a missing key.
            let method = match step.details.get("method") {
                None => "POST",
                Some(_) => setting(step, "method")?.unwrap_or(""),
            };
            let authenticated = setting(step, "credential")?.is_some_and(php::is_truthy_str);
            format!(
                "Make an {}HTTP {} request to {}.",
                if authenticated { "authenticated " } else { "" },
                description_value(method),
                description_value(&domain)
            )
        }
        "HarbormasterArcLintBuildStepImplementation" => "Automatic `arc lint` step.".to_owned(),
        "HarbormasterArcUnitBuildStepImplementation" => "Automatic `arc unit` step.".to_owned(),
        "HarbormasterAbortOlderBuildsBuildStepImplementation" => {
            "When building a revision, abort copies of this build plan which are currently \
             running against older diffs."
                .to_owned()
        }
        "HarbormasterWaitForPreviousBuildStepImplementation" => {
            "Wait for previous commits to finish building the current plan before continuing."
                .to_owned()
        }
        other => diff_html::escape(&format!(
            "This step has an invalid implementation ({other})."
        )),
    })
}

/// `HarbormasterBuildStepImplementation::getDependencies()`: the steps of
/// the `dependsOn` setting (no implementation takes artifacts).
fn dependencies(step: &Step) -> Result<Vec<Phid>, PageError> {
    match step.details.get("dependsOn") {
        None => Ok(Vec::new()),
        Some(Value::Array(phids)) => phids
            .iter()
            .map(|phid| match phid {
                Value::String(phid) => Ok(Phid::new(phid.clone())),
                _ => Err(PageError::Fallback("an unusual step dependency")),
            })
            .collect(),
        // `array_merge()` of anything else fails.
        Some(_) => Err(PageError::Fallback("an unusual step dependency")),
    }
}

/// `AbstractDirectedGraph::getNodesInRoughTopologicalOrder()`: rounds of
/// the steps whose dependencies all ran, in the order of the query, then
/// the steps which never can run, as cycles. Each is `(index, depth,
/// cycle)`.
fn run_order(edges: &[Vec<Phid>], phids: &[Phid]) -> Vec<(usize, usize, bool)> {
    let mut completed: Vec<bool> = vec![false; phids.len()];
    let is_completed = |completed: &[bool], phid: &Phid| {
        phids
            .iter()
            .position(|node| node == phid)
            .is_some_and(|index| completed[index])
    };
    let mut order = Vec::new();
    let mut depth = 0;
    loop {
        let next: Vec<usize> = (0..phids.len())
            .filter(|&index| {
                !completed[index]
                    && edges[index]
                        .iter()
                        .all(|edge| is_completed(&completed, edge))
            })
            .collect();
        if next.is_empty() {
            break;
        }
        for index in next {
            order.push((index, depth, false));
            completed[index] = true;
        }
        depth += 1;
    }
    for (index, done) in completed.iter().enumerate() {
        if !done {
            order.push((index, depth, true));
        }
    }
    order
}

/// `buildStepList()`: the steps in the order they run, numbered by round,
/// and whether the plan deadlocks.
fn step_items(steps: &[Step]) -> Result<(Vec<StepItem>, bool), PageError> {
    let invalid = |step: &Step| harbormaster::implementation_name(&step.class_name).is_none();
    // `getAvailableArtifacts()`, which every valid step calls, fails on
    // the steps of an unknown implementation.
    if steps.iter().any(invalid) && !steps.iter().all(invalid) {
        return Err(PageError::Fallback("a step of an unknown implementation"));
    }
    let phids: Vec<Phid> = steps.iter().map(|step| step.phid.clone()).collect();
    let edges: Vec<Vec<Phid>> = steps
        .iter()
        .map(|step| {
            if invalid(step) {
                Ok(Vec::new())
            } else {
                dependencies(step)
            }
        })
        .collect::<Result<_, _>>()?;
    let mut items = Vec::new();
    let (mut last_depth, mut position) = (0, 1);
    let mut deadlocks = false;
    for (index, depth, cycle) in run_order(&edges, &phids) {
        let step = &steps[index];
        let depth = depth + 1;
        if depth == last_depth {
            position += 1;
        } else {
            last_depth = depth;
            position = 1;
        }
        let name = harbormaster::step_name(step)
            .ok_or(PageError::Fallback("a step of an unknown implementation"))?;
        let depends = edges[index]
            .iter()
            .map(|phid| {
                steps
                    .iter()
                    .find(|other| other.phid == *phid)
                    .map(|other| harbormaster::step_name(other).unwrap_or_default())
            })
            .collect();
        deadlocks |= cycle;
        items.push(StepItem {
            id: step.id,
            label: format!("Step {depth}.{position}"),
            name,
            description_html: step_description(step)?,
            depends,
            warning: invalid(step) || deadlocks,
        });
    }
    Ok((items, deadlocks))
}

/// `HeraldAdapter::getAdapterContentName()` of the adapters which run
/// build plans.
fn adapter_name(content_type: &str) -> Option<&'static str> {
    Some(match content_type {
        "differential" => "Differential Revisions",
        "differential.diff" => "Differential Diffs",
        "commit" => "Commits",
        _ => return None,
    })
}

/// The Herald rules running the plan the viewer can see
/// (`HeraldRuleListView`), and whether there are more.
async fn rules<S: WebState>(
    state: &S,
    viewer: &WebUser,
    scope: &Scope,
    plan: &Phid,
) -> Result<(Vec<RuleItem>, bool), PageError> {
    if !super::can_use(state, viewer, application::HERALD).await? {
        return Ok((Vec::new(), false));
    }
    let database = state.database();
    let found = harbormaster::rules_affecting(database, plan).await?;
    if found.iter().any(|rule| rule.rule_type == "object") {
        return Err(PageError::Fallback("an object rule running a plan"));
    }
    if found
        .iter()
        .any(|rule| adapter_name(&rule.content_type).is_none())
    {
        return Err(PageError::Fallback(
            "a rule of another adapter running a plan",
        ));
    }
    let rules: Vec<phab_app_herald::Rule> = found
        .iter()
        .map(|rule| phab_app_herald::Rule {
            id: rule.id,
            phid: rule.phid.clone(),
            name: rule.name.clone(),
            content_type: rule.content_type.clone(),
            rule_type: rule.rule_type.clone(),
            is_disabled: rule.is_disabled,
            trigger_object_phid: rule.trigger_object_phid.clone(),
        })
        .collect();
    let visible: HashSet<Phid> = phab_app_herald::visible(
        database,
        state.config(),
        &scope.viewer(),
        rules,
        &HashSet::<Phid>::new(),
        state.now(),
    )
    .await
    .map_err(super::internal)?
    .into_iter()
    .map(|rule| rule.phid)
    .collect();
    let mut shown: Vec<_> = found
        .into_iter()
        .filter(|rule| visible.contains(&rule.phid))
        .collect();
    let more = shown.len() > LIST_LIMIT;
    shown.truncate(LIST_LIMIT);

    // `validateRuleAuthors()`: the author of a personal rule must exist
    // and be active.
    let personal: Vec<Phid> = shown
        .iter()
        .filter(|rule| rule.rule_type == "personal")
        .map(|rule| rule.author_phid.clone())
        .collect();
    let users = phab_auth::user::load_users_by_phid(database, &personal).await?;
    let handles = super::load_handles(state, scope, &personal).await?;
    let items = shown
        .iter()
        .map(|rule| {
            let (kind, icon) = match rule.rule_type.as_str() {
                "personal" => ("Personal Rule", "fa-user"),
                _ => ("Global Rule", "fa-globe"),
            };
            let is_personal = rule.rule_type == "personal";
            let active = !is_personal
                || users
                    .iter()
                    .any(|user| user.phid == rule.author_phid && user.is_activated(state.config()));
            RuleItem {
                id: rule.id,
                name: rule.name.clone(),
                kind,
                icon,
                author_html: is_personal
                    .then(|| super::handle(&handles, &rule.author_phid).short_link_html()),
                author_inactive: !active,
                affects: adapter_name(&rule.content_type).unwrap_or(""),
            }
        })
        .collect();
    Ok((items, more))
}

/// `HarbormasterBuildView`: the latest builds of the plan.
async fn recent_builds<S: WebState>(
    state: &S,
    scope: &Scope,
    plan: &Plan,
) -> Result<(Vec<BuildItem>, bool), PageError> {
    let request = scope.request();
    let page = harbormaster::builds_of_plan(&request, &plan.phid, LIST_LIMIT)
        .await
        .map_err(method_error)?;
    let buildables = harbormaster::buildables_by_phid(
        &request,
        &page
            .items
            .iter()
            .map(|build| build.buildable_phid.clone())
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(method_error)?;
    let mut phids: Vec<Phid> = buildables
        .iter()
        .map(|buildable| buildable.object_phid.clone())
        .collect();
    phids.extend(
        page.items
            .iter()
            .filter_map(|build| build.initiator_phid.as_deref())
            .filter(|phid| php::is_truthy_str(phid))
            .map(Phid::new),
    );
    let handles = super::load_handles(state, scope, &phids).await?;
    let plans = std::collections::HashMap::from([(plan.phid.clone(), plan.clone())]);
    let items = page
        .items
        .iter()
        .map(|build| {
            let object = buildables
                .iter()
                .find(|buildable| buildable.phid == build.buildable_phid)
                .map_or_else(String::new, |buildable| {
                    super::handle(&handles, &buildable.object_phid).name.clone()
                });
            BuildItem {
                id: build.id,
                name: super::build_name(build, &plans),
                object,
                initiator_html: build
                    .initiator_phid
                    .as_deref()
                    .filter(|phid| php::is_truthy_str(phid))
                    .map(|phid| super::handle(&handles, &Phid::new(phid)).link_html()),
                date: crate::time::format_utc(build.dates.0),
                status: status::build(&build.status),
            }
        })
        .collect();
    Ok((items, page.next.is_some()))
}

/// The display name of a behavior option, or the stored value.
fn option_name(behavior: &str, value: &Value) -> String {
    let key = value.as_str().unwrap_or("");
    BEHAVIORS
        .iter()
        .find(|(name, _, _)| *name == behavior)
        .and_then(|(_, _, options)| options.iter().find(|option| option.0 == key))
        .map_or_else(|| key.to_owned(), |option| option.1.to_owned())
}

/// `HarbormasterBuildPlanTransaction` titles, and the core ones plans
/// have when they are created (`shouldHide()`).
fn title(
    transaction: &harbormaster::Transaction,
    author: &str,
) -> Result<Option<String>, PageError> {
    let is_create = transaction.metadata_flag("core.create");
    let old_is_empty = match &transaction.old_value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Number(number) => number.as_i64() == Some(0),
        Value::Bool(_) => false,
    };
    let value = |value: &Value| diff_html::escape(value.as_str().unwrap_or(""));
    let kind = transaction.transaction_type.as_str();
    let is_policy = matches!(kind, "core:view-policy" | "core:edit-policy");
    if is_create && transaction.metadata_flag("core.default") {
        return Ok(None);
    }
    if is_create && !is_policy && kind != "core:create" && old_is_empty {
        return Ok(None);
    }
    if is_policy && !is_create && transaction.old_value.is_null() {
        return Ok(None);
    }
    Ok(Some(match kind {
        "core:create" => format!("{author} created this object."),
        "harbormaster:name" => format!(
            "{author} renamed this build plan from \"{}\" to \"{}\".",
            value(&transaction.old_value),
            value(&transaction.new_value)
        ),
        "harbormaster:status" => {
            if transaction.new_value.as_str() == Some("disabled") {
                format!("{author} disabled this build plan.")
            } else {
                format!("{author} enabled this build plan.")
            }
        }
        "behavior" => {
            let behavior = transaction
                .metadata
                .get("behavior-key")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = BEHAVIORS
                .iter()
                .find(|(key, _, _)| *key == behavior)
                .map_or(behavior, |(_, name, _)| *name);
            format!(
                "{author} changed the <em>{}</em> behavior for this plan from <em>{}</em> to \
                 <em>{}</em>.",
                diff_html::escape(name),
                diff_html::escape(&option_name(behavior, &transaction.old_value)),
                diff_html::escape(&option_name(behavior, &transaction.new_value))
            )
        }
        _ => return Err(PageError::Fallback("a build plan transaction not ported")),
    }))
}

/// `GET /harbormaster/plan/{id}/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<PlanPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let id: i64 = (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
        .ok_or(PageError::NotFound)?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let request = scope.request();
    let plan = match harbormaster::plan(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(plan) => plan,
        Lookup::Missing => return Err(PageError::NotFound),
        Lookup::Denied => return Err(PageError::PolicyDenied),
    };
    let can_edit = harbormaster::can_edit_plans(&request, std::slice::from_ref(&plan))
        .await
        .map_err(method_error)?
        .first()
        .copied()
        .unwrap_or(false);

    let steps = harbormaster::steps_of_plan(&request, &plan.phid)
        .await
        .map_err(method_error)?;
    let (steps, deadlocks) = step_items(&steps)?;
    let error = if steps.is_empty() {
        Some(
            "This build plan does not have any build steps yet, so it will not do anything \
             when run.",
        )
    } else if deadlocks {
        Some(
            "This build plan will deadlock when executed, due to circular dependencies present \
             in the build plan. Examine the step list and resolve the deadlock.",
        )
    } else {
        None
    };

    let (rules, more_rules) = rules(state.as_ref(), &viewer, &scope, &plan.phid).await?;
    let (builds, more_builds) = recent_builds(state.as_ref(), &scope, &plan).await?;
    let timeline = super::timeline(
        state.as_ref(),
        &scope,
        TransactionTable::Plan,
        &plan.phid,
        title,
    )
    .await?;

    let disabled = plan.status == "disabled";
    let actions = actions(&plan, can_edit);
    let name = harbormaster::plan_name(&plan);
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(PlanPage {
        shell: Shell::for_user(state.config(), &viewer, &name, current),
        id,
        plan_phid: plan.phid.as_str().to_owned(),
        name,
        disabled,
        error,
        can_edit,
        steps,
        behaviors: behavior_rows(&plan),
        rules,
        more_rules,
        builds,
        more_builds,
        timeline,
        actions,
    }))
}

/// `buildCurtainView()`.
fn actions(plan: &Plan, can_edit: bool) -> Vec<Action> {
    let id = plan.id;
    // `hasRunCapability()` and `canRunManually()`.
    let can_run = (can_run_without_edit_capability(plan) || can_edit) && plan.auto_key.is_none();
    vec![
        Action {
            name: "Edit Plan",
            icon: "fa-pencil",
            uri: format!("/harbormaster/plan/edit/{id}/"),
            enabled: can_edit,
        },
        if plan.status == "disabled" {
            Action {
                name: "Enable Plan",
                icon: "fa-check",
                uri: format!("/harbormaster/plan/disable/{id}/"),
                enabled: can_edit,
            }
        } else {
            Action {
                name: "Disable Plan",
                icon: "fa-ban",
                uri: format!("/harbormaster/plan/disable/{id}/"),
                enabled: can_edit,
            }
        },
        Action {
            name: "Run Plan Manually",
            icon: "fa-play-circle",
            uri: format!("/harbormaster/plan/run/{id}/"),
            enabled: can_run,
        },
    ]
}

/// `newOptionsView()`: each behavior and the plan's option.
fn behavior_rows(plan: &Plan) -> Vec<BehaviorRow> {
    BEHAVIORS
        .iter()
        .map(|(key, name, _)| {
            let (_, option, icon, description) = plan_option(plan, key);
            let (icon, color) = icon.split_once(' ').unwrap_or((icon, ""));
            BehaviorRow {
                key,
                icon,
                color,
                name,
                option,
                description,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn plan(properties: Value) -> Plan {
        Plan {
            id: 1,
            phid: Phid::new("PHID-HMCP-1"),
            name: "p".to_owned(),
            status: "active".to_owned(),
            auto_key: None,
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            properties: match properties {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            dates: (1, 1),
        }
    }

    #[test]
    fn behaviors_fall_back_to_their_default() {
        let plan = plan(json!({"behavior.runnable": "view", "behavior.restartable": "odd"}));
        assert!(can_run_without_edit_capability(&plan));
        assert_eq!(behavior_option(&plan, "restartable"), "always");
        assert_eq!(behavior_option(&plan, "hold-drafts"), "always");
    }

    #[test]
    fn http_step_descriptions() {
        let step = |details: Value| Step {
            id: 1,
            phid: Phid::new("PHID-HMCS-1"),
            plan_phid: Phid::new("PHID-HMCP-1"),
            class_name: "HarbormasterHTTPRequestBuildStepImplementation".to_owned(),
            details: match details {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            name: None,
            description: String::new(),
            auto_key: None,
            dates: (1, 1),
            policies: None,
        };
        assert_eq!(
            step_description(&step(json!({"uri": "https://ci.example:8080/x?y"}))).unwrap(),
            "Make an HTTP <strong>POST</strong> request to <strong>ci.example</strong>."
        );
        assert_eq!(
            step_description(&step(json!({"method": "GET", "credential": "PHID-CDTL-1"}))).unwrap(),
            "Make an authenticated HTTP <strong>GET</strong> request to <em>(null)</em>."
        );
    }
}
