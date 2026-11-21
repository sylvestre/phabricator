//! `harbormaster.build.search` (`HarbormasterBuildSearchEngine`) and the
//! legacy `harbormaster.querybuilds`, which calls it.
//!
//! Ported: every constraint (typeahead functions aside), the builtin
//! orders, paging, and the `querybuilds` attachment, for builds of
//! buildables of diffs and commits. Left to PHP: typeahead functions like
//! `viewer()`, saved queries and order vectors.

use std::collections::HashMap;

use phab_conduit::pager::Paging;
use phab_conduit::search::{self, Clauses};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use serde_json::{Map, Value, json};

use crate::constraints::{self, Ids};
use crate::query::{Order, Query};
use crate::storage::{self, Build, Plan};
use crate::visibility;

const VALID_CONSTRAINTS: &[&str] = &[
    "plans",
    "buildables",
    "statuses",
    "initiators",
    "ids",
    "phids",
];

const ATTACHMENTS: &[&str] = &["querybuilds"];

/// `HarbormasterBuildStatus::getBuildStatusSpec()`: the name and the ANSI
/// color of a status.
pub(crate) fn status_spec(status: &str) -> (String, &'static str) {
    let (name, ansi) = match status {
        "inactive" => ("Inactive", "yellow"),
        "pending" => ("Pending", "yellow"),
        "building" => ("Building", "yellow"),
        "passed" => ("Passed", "green"),
        "failed" => ("Failed", "red"),
        "aborted" => ("Aborted", "red"),
        "error" => ("Unexpected Error", "red"),
        "paused" => ("Paused", "yellow"),
        "deadlocked" => ("Deadlocked", "red"),
        "x-pausing" => ("Pausing", "red"),
        "x-resuming" => ("Resuming", "red"),
        "x-restarting" => ("Restarting", "red"),
        "x-aborting" => ("Aborting", "red"),
        other => return (format!("Unknown (\"{other}\")"), "magenta"),
    };
    (name.to_owned(), ansi)
}

/// The query of a search: its constraints, in field order.
fn filters(params: &Map<String, Value>, strict: bool) -> Result<Clauses, MethodError> {
    let constraints = search::Constraints::parse(params.get("constraints"), VALID_CONSTRAINTS)?;
    let plans = constraints::datasource(&constraints, "plans")?;
    let buildables = constraints::phid_list(&constraints, "buildables")?;
    let statuses = constraints::datasource(&constraints, "statuses")?;
    let initiators = constraints::datasource(&constraints, "initiators")?;
    let ids = Ids::parse(&constraints, strict)?;
    let mut clauses = Clauses::default();
    ids.add_conditions(&mut clauses, "b");
    constraints::in_list(&mut clauses, "b.buildStatus", statuses.as_ref());
    constraints::in_list(&mut clauses, "b.buildablePHID", buildables.as_ref());
    constraints::in_list(&mut clauses, "b.buildPlanPHID", plans.as_ref());
    constraints::in_list(&mut clauses, "b.initiatorPHID", initiators.as_ref());
    Ok(clauses)
}

/// `HarbormasterBuild::getName()`: the name of its plan, if the viewer
/// can see it (`didFilterPage()` attaches it).
fn name(build: &Build, plans: &HashMap<Phid, Plan>) -> String {
    plans
        .get(&build.plan_phid)
        .map_or_else(|| "Build".to_owned(), |plan| plan.name.clone())
}

/// One build: `getFieldValuesForConduit()`, the extensions' fields and
/// the attachments asked for.
fn result(
    request: &ApiRequest<'_>,
    build: &Build,
    plans: &HashMap<Phid, Plan>,
    querybuilds: bool,
) -> Result<Value, MethodError> {
    let policies = storage::attached(build.policies.as_ref())?;
    let (status_name, ansi) = status_spec(&build.status);
    let name = name(build, plans);
    let initiator = build
        .initiator_phid
        .as_deref()
        .filter(|phid| php::is_truthy_str(phid));
    let mut fields = Map::new();
    fields.insert(
        "buildablePHID".to_owned(),
        json!(build.buildable_phid.as_str()),
    );
    fields.insert("buildPlanPHID".to_owned(), json!(build.plan_phid.as_str()));
    fields.insert(
        "buildStatus".to_owned(),
        json!({"value": build.status, "name": status_name, "color.ansi": ansi}),
    );
    fields.insert("initiatorPHID".to_owned(), json!(initiator));
    fields.insert("name".to_owned(), json!(name));
    let mut attachments = Map::new();
    if querybuilds {
        attachments.insert(
            "querybuilds".to_owned(),
            json!({
                "uri": crate::production_uri(
                    request,
                    &format!("/harbormaster/build/{}/", build.id),
                )?,
                "name": name,
                "buildStatusName": status_name,
            }),
        );
    }
    Ok(crate::search_result(
        build.id,
        build.phid.as_str(),
        fields,
        build.dates,
        policies.view_and_edit(),
        attachments,
    ))
}

/// `buildConduitResponse()` of the engine for `params`, which are those
/// of `request` unless `harbormaster.querybuilds` makes the call.
async fn search(
    request: &ApiRequest<'_>,
    params: &Map<String, Value>,
    strict: bool,
) -> Result<Value, MethodError> {
    match params.get("queryKey") {
        None | Some(Value::Null) => {}
        Some(Value::String(key)) if key.is_empty() => {}
        Some(_) => return Err(MethodError::Unsupported("saved or builtin queries")),
    }
    let clauses = filters(params, strict)?;
    let requested = search::requested_attachments(params.get("attachments"), ATTACHMENTS)?;
    let order = Order::parse(params.get("order"), "HarbormasterBuildQuery", false)?;
    let paging = Paging::for_search(
        params.get("limit"),
        params.get("after"),
        params.get("before"),
    )?;
    let page = Query::<Build>::new(request, clauses, order)
        .page(&paging)
        .await?;
    let plans = visibility::visible_plans_by_phid(
        request,
        storage::phid_strings(page.items.iter().map(|build| &build.plan_phid)),
    )
    .await?;
    let data: Vec<Value> = page
        .items
        .iter()
        .map(|build| result(request, build, &plans, !requested.is_empty()))
        .collect::<Result<_, _>>()?;
    let cursor = |build: &Option<Build>| build.as_ref().map(|build| build.id.to_string());
    Ok(json!({
        "data": data,
        "maps": {},
        "query": {"queryKey": null},
        "cursor": {
            "limit": paging.size.shown,
            "after": cursor(&page.next),
            "before": cursor(&page.previous),
            "order": params.get("order").cloned().unwrap_or(Value::Null),
        },
    }))
}

/// `harbormaster.build.search`.
pub struct BuildSearch;

impl Method for BuildSearch {
    fn name(&self) -> &'static str {
        "harbormaster.build.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move { search(request, &request.params, request.strictly_typed).await })
    }
}

/// `harbormaster.querybuilds`: a `harbormaster.build.search` call (a
/// `ConduitCall`, so strictly typed) with the `querybuilds` attachment,
/// its results flattened.
pub struct QueryBuilds;

impl Method for QueryBuilds {
    fn name(&self) -> &'static str {
        "harbormaster.querybuilds"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "ids",
            "phids",
            "buildStatuses",
            "buildablePHIDs",
            "buildPlanPHIDs",
            "before",
            "after",
            "limit",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            // array_filter() drops the falsy values.
            let mut constraints = Map::new();
            for (key, param) in [
                ("ids", "ids"),
                ("phids", "phids"),
                ("statuses", "buildStatuses"),
                ("buildables", "buildablePHIDs"),
                ("plans", "buildPlanPHIDs"),
            ] {
                if let Some(value) = request.value(param).filter(|value| php::is_truthy(value)) {
                    constraints.insert(key.to_owned(), value.clone());
                }
            }
            let mut params = Map::new();
            if !constraints.is_empty() {
                params.insert("constraints".to_owned(), Value::Object(constraints));
            }
            params.insert("attachments".to_owned(), json!({"querybuilds": true}));
            for key in ["limit", "before", "after"] {
                if let Some(value) = request.value(key).filter(|value| php::is_truthy(value)) {
                    params.insert(key.to_owned(), value.clone());
                }
            }
            let mut subsumption = search(request, &params, true).await?;
            let data = subsumption["data"].as_array().cloned().unwrap_or_default();
            let flattened: Vec<Value> = data
                .into_iter()
                .map(|build| {
                    let Value::Object(mut build) = build else {
                        return Err(MethodError::internal("a build which is not a map"));
                    };
                    let fields = build.remove("fields");
                    let attachments = build.remove("attachments");
                    let mut merged = build;
                    // array_mergev(): later keys overwrite earlier ones, in
                    // their first position.
                    if let Some(Value::Object(querybuilds)) =
                        attachments.and_then(|mut map| map.get_mut("querybuilds").map(Value::take))
                    {
                        merged.extend(querybuilds);
                    }
                    if let Some(Value::Object(mut fields)) = fields {
                        if let Some(Value::Object(status)) = fields.get_mut("buildStatus") {
                            status.remove("color.ansi");
                        }
                        merged.extend(fields);
                    }
                    Ok(Value::Object(merged))
                })
                .collect::<Result<_, MethodError>>()?;
            subsumption["data"] = Value::Array(flattened);
            Ok(subsumption)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_like_php() {
        assert_eq!(status_spec("passed"), ("Passed".to_owned(), "green"));
        assert_eq!(status_spec("error").0, "Unexpected Error");
        assert_eq!(
            status_spec("nope"),
            ("Unknown (\"nope\")".to_owned(), "magenta")
        );
    }
}
