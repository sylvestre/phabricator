//! `harbormaster.buildplan.search` (`HarbormasterBuildPlanSearchEngine`)
//! and `harbormaster.step.search` (`HarbormasterBuildStepSearchEngine`).
//!
//! Ported: the `status`, `ids` and `phids` constraints (and an empty
//! `match`), the builtin orders (`name` for plans), paging, and the
//! `subscribers` and `projects` attachments of plans. Left to PHP: name
//! searches (ngrams), the `subscribers` and `projects` constraints, steps
//! without a name whose implementation is unknown, saved queries and order
//! vectors.

use std::collections::HashMap;

use phab_conduit::pager::Paging;
use phab_conduit::search::{self, Clauses};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::edges;
use serde_json::{Map, Value, json};

use crate::constraints::{self, Ids};
use crate::query::{Order, Query};
use crate::storage::{self, Plan, Step};

const PLAN_CONSTRAINTS: &[&str] = &["match", "status", "ids", "phids", "subscribers", "projects"];

const PLAN_ATTACHMENTS: &[&str] = &["subscribers", "projects"];

/// The most subscribers the `subscribers` attachment lists.
const MAX_SUBSCRIBERS: usize = 10;

/// `HarbormasterBuildPlanBehavior::newPlanBehaviors()`: each behavior's
/// key, its options, and the default one.
const BEHAVIORS: &[(&str, &[&str], &str)] = &[
    ("hold-drafts", &["always", "building", "never"], "always"),
    (
        "arc-land",
        &["always", "building", "complete", "never"],
        "always",
    ),
    ("buildable", &["always", "building", "never"], "always"),
    ("restartable", &["always", "failed", "never"], "always"),
    ("runnable", &["edit", "view"], "edit"),
];

/// `HarbormasterBuildPlanBehavior::getPlanOption()`: the stored option of
/// a behavior if it is one, the default otherwise.
fn option(
    plan: &Plan,
    (key, options, default): (&str, &[&'static str], &'static str),
) -> Result<&'static str, MethodError> {
    match plan.properties.get(&format!("behavior.{key}")) {
        Some(Value::String(value)) => Ok(options
            .iter()
            .find(|option| *option == value)
            .copied()
            .unwrap_or(default)),
        // isset() with an array offset raises a TypeError.
        Some(Value::Array(_) | Value::Object(_)) => {
            Err(MethodError::Unsupported("unusual plan behaviors"))
        }
        // Integer keys (booleans and floats cast to them) are never
        // options.
        _ => Ok(default),
    }
}

/// [`option()`] of the behavior `key`.
pub(crate) fn behavior(plan: &Plan, key: &str) -> Result<&'static str, MethodError> {
    let (key, options, default) = BEHAVIORS
        .iter()
        .find(|(known, _, _)| *known == key)
        .ok_or_else(|| MethodError::internal(format!("no plan behavior \"{key}\"")))?;
    option(plan, (key, options, default))
}

/// `getPlanOption()` of each behavior.
fn behaviors(plan: &Plan) -> Result<Value, MethodError> {
    let mut map = Map::new();
    for (key, options, default) in BEHAVIORS {
        let option = option(plan, (key, options, default))?;
        map.insert((*key).to_owned(), json!({"value": option}));
    }
    Ok(Value::Object(map))
}

/// The plans' subscribers (`PhabricatorSubscriptionsSearchEngineAttachment`).
async fn subscribers(
    request: &ApiRequest<'_>,
    plans: &[Plan],
) -> Result<HashMap<Phid, Value>, MethodError> {
    let database = &request.context.database;
    let phids: Vec<Phid> = plans.iter().map(|plan| plan.phid.clone()).collect();
    let mut subscribed: HashMap<Phid, Vec<Phid>> = HashMap::new();
    for edge in edges::load_edges(database, &phids, &[edges::OBJECT_HAS_SUBSCRIBER], None).await? {
        let list = subscribed.entry(edge.src).or_default();
        if !list.contains(&edge.dst) {
            list.push(edge.dst);
        }
    }
    let viewer = request.viewer.phid.clone();
    Ok(plans
        .iter()
        .map(|plan| {
            let list = subscribed.get(&plan.phid).map_or(&[][..], Vec::as_slice);
            // Nobody is automatically subscribed to a plan.
            let viewer_is_subscribed = viewer.as_ref().is_some_and(|viewer| list.contains(viewer));
            let shown: Vec<&str> = list
                .iter()
                .take(MAX_SUBSCRIBERS)
                .map(Phid::as_str)
                .collect();
            (
                plan.phid.clone(),
                json!({
                    "subscriberPHIDs": shown,
                    "subscriberCount": list.len(),
                    "viewerIsSubscribed": viewer_is_subscribed,
                }),
            )
        })
        .collect())
}

/// The plans' projects (`PhabricatorProjectsSearchEngineAttachment`).
async fn projects(
    request: &ApiRequest<'_>,
    plans: &[Plan],
) -> Result<HashMap<Phid, Value>, MethodError> {
    let phids: Vec<Phid> = plans.iter().map(|plan| plan.phid.clone()).collect();
    let mut projects: HashMap<Phid, Vec<Phid>> = HashMap::new();
    for edge in edges::load_edges(
        &request.context.database,
        &phids,
        &[edges::OBJECT_HAS_PROJECT],
        None,
    )
    .await?
    {
        let list = projects.entry(edge.src).or_default();
        if !list.contains(&edge.dst) {
            list.push(edge.dst);
        }
    }
    Ok(plans
        .iter()
        .map(|plan| {
            let list: Vec<&str> = projects
                .get(&plan.phid)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(Phid::as_str)
                .collect();
            (plan.phid.clone(), json!({"projectPHIDs": list}))
        })
        .collect())
}

/// `harbormaster.buildplan.search`.
pub struct BuildPlanSearch;

impl Method for BuildPlanSearch {
    fn name(&self) -> &'static str {
        "harbormaster.buildplan.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let constraints = constraints::parse(request, PLAN_CONSTRAINTS)?;
            let strict = request.strictly_typed;
            // withNameNgrams() ignores an empty string.
            if constraints::text(&constraints, "match")?.is_some_and(|text| !text.is_empty()) {
                return Err(MethodError::Unsupported("build plan name searches"));
            }
            let statuses = constraints::string_list(&constraints, "status")?;
            let ids = Ids::parse(&constraints, strict)?;
            if constraints.has(&["subscribers", "projects"]) {
                return Err(MethodError::Unsupported(
                    "subscriber and project constraints",
                ));
            }
            let mut clauses = Clauses::default();
            ids.add_conditions(&mut clauses, "plan");
            constraints::in_list(&mut clauses, "plan.planStatus", statuses.as_ref());
            let requested =
                search::requested_attachments(request.value("attachments"), PLAN_ATTACHMENTS)?;
            let order = Order::parse(request.value("order"), "HarbormasterBuildPlanQuery", true)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let page = Query::<Plan>::new(request, clauses, order)
                .page(&paging)
                .await?;
            let mut attachment_data: Vec<(&str, HashMap<Phid, Value>)> = Vec::new();
            if !page.items.is_empty() {
                for key in &requested {
                    let data = if *key == "subscribers" {
                        subscribers(request, &page.items).await?
                    } else {
                        projects(request, &page.items).await?
                    };
                    attachment_data.push((key, data));
                }
            }
            let most_open = crate::most_open(&request.context.config);
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|plan| {
                    let mut fields = Map::new();
                    fields.insert("name".to_owned(), json!(plan.name));
                    fields.insert("status".to_owned(), json!({"value": plan.status}));
                    fields.insert("behaviors".to_owned(), behaviors(plan)?);
                    let attachments: Map<String, Value> = attachment_data
                        .iter()
                        .map(|(key, data)| {
                            (
                                (*key).to_owned(),
                                data.get(&plan.phid).cloned().unwrap_or_default(),
                            )
                        })
                        .collect();
                    Ok(crate::search_result(
                        plan.id,
                        plan.phid.as_str(),
                        fields,
                        plan.dates,
                        plan.policies(most_open).view_and_edit(),
                        attachments,
                    ))
                })
                .collect::<Result<_, MethodError>>()?;
            Ok(search::response(request, &data, &paging, &page, |plan| {
                plan.id.to_string()
            }))
        })
    }
}

/// `HarbormasterBuildStep::getName()`: its name, or its implementation's.
fn step_name(step: &Step) -> Result<String, MethodError> {
    match &step.name {
        Some(name) if !name.is_empty() => Ok(name.clone()),
        _ => crate::step_implementation(&step.class_name)
            .map(|(name, _)| name.to_owned())
            .ok_or(MethodError::Unsupported("steps of unknown implementations")),
    }
}

/// `harbormaster.step.search`.
pub struct StepSearch;

impl Method for StepSearch {
    fn name(&self) -> &'static str {
        "harbormaster.step.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let constraints = constraints::parse(request, &["ids", "phids"])?;
            let ids = Ids::parse(&constraints, request.strictly_typed)?;
            let mut clauses = Clauses::default();
            ids.add_conditions(&mut clauses, "step");
            search::requested_attachments(request.value("attachments"), &[])?;
            let order = Order::parse(request.value("order"), "HarbormasterBuildStepQuery", false)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let page = Query::<Step>::new(request, clauses, order)
                .page(&paging)
                .await?;
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|step| {
                    let policies = storage::attached(step.policies.as_ref())?;
                    let mut fields = Map::new();
                    fields.insert("name".to_owned(), json!(step_name(step)?));
                    fields.insert("description".to_owned(), json!({"raw": step.description}));
                    fields.insert("buildPlanPHID".to_owned(), json!(step.plan_phid.as_str()));
                    Ok(crate::search_result(
                        step.id,
                        step.phid.as_str(),
                        fields,
                        step.dates,
                        policies.view_and_edit(),
                        Map::new(),
                    ))
                })
                .collect::<Result<_, MethodError>>()?;
            Ok(search::response(request, &data, &paging, &page, |step| {
                step.id.to_string()
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(properties: Value) -> Plan {
        let Value::Object(properties) = properties else {
            panic!("properties");
        };
        Plan {
            id: 1,
            phid: Phid::new("PHID-HMCP-1"),
            name: "p".to_owned(),
            status: "active".to_owned(),
            auto_key: None,
            view_policy: "users".to_owned(),
            edit_policy: "admin".to_owned(),
            properties,
            dates: (1, 1),
        }
    }

    #[test]
    fn behaviors_like_php() {
        assert_eq!(
            behaviors(&plan(json!({
                "behavior.hold-drafts": "never",
                "behavior.arc-land": "nope",
                "behavior.runnable": true,
            })))
            .unwrap(),
            json!({
                "hold-drafts": {"value": "never"},
                "arc-land": {"value": "always"},
                "buildable": {"value": "always"},
                "restartable": {"value": "always"},
                "runnable": {"value": "edit"},
            })
        );
        assert!(behaviors(&plan(json!({"behavior.buildable": ["x"]}))).is_err());
    }
}
