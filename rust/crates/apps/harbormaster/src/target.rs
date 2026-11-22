//! `harbormaster.target.search` (`HarbormasterBuildTargetSearchEngine`),
//! `harbormaster.artifact.search` (`HarbormasterArtifactSearchEngine`) and
//! `harbormaster.log.search` (`HarbormasterBuildLogSearchEngine`).
//!
//! Ported: every constraint, the builtin orders and paging. Left to PHP:
//! typeahead functions, dates small enough to be read as `Ymd`, the
//! `phids` constraint of artifacts (`HarbormasterBuildArtifactQuery` has
//! no `withPHIDs()`, so PHP fails), saved queries and order vectors.

use phab_conduit::pager::Paging;
use phab_conduit::search::{self, Clauses};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use serde_json::{Map, Value, json};

use crate::constraints::{self, Ids};
use crate::query::{Order, Query};
use crate::storage::{self, Artifact, Log, Object, Target};

const TARGET_CONSTRAINTS: &[&str] = &[
    "buildPHIDs",
    "createdStart",
    "createdEnd",
    "startedStart",
    "startedEnd",
    "completedStart",
    "completedEnd",
    "statuses",
    "ids",
    "phids",
];

const BY_TARGET_CONSTRAINTS: &[&str] = &["buildTargetPHIDs", "ids", "phids"];

/// `HarbormasterBuildTarget::getBuildTargetStatusName()`.
fn status_name(status: &str) -> &'static str {
    match status {
        "target/pending" => "Pending",
        "target/building" => "Building",
        "target/waiting" => "Waiting for Message",
        "target/passed" => "Passed",
        "target/failed" => "Failed",
        "target/aborted" => "Aborted",
        _ => "Unknown",
    }
}

/// `HarbormasterBuildTarget::getName()`: its name, unless it is an
/// autotarget, whose name is its implementation's (or its class, when the
/// implementation doesn't exist).
pub(crate) fn target_name(target: &Target) -> String {
    let implementation = crate::step_implementation(&target.class_name);
    let is_autotarget = implementation.is_some_and(|(_, key)| key.is_some());
    match (&target.name, implementation) {
        (Some(name), _) if !name.is_empty() && !is_autotarget => name.clone(),
        (_, Some((name, _))) => name.to_owned(),
        (_, None) => target.class_name.clone(),
    }
}

/// An epoch of `getFieldValuesForConduit()`: falsy is `null`.
fn epoch(value: Option<i64>) -> Value {
    value
        .filter(|epoch| *epoch != 0)
        .map_or(Value::Null, |epoch| json!(epoch))
}

/// What the searches share once their conditions are built.
async fn respond<T: Object>(
    request: &ApiRequest<'_>,
    clauses: Clauses,
    query_class: &str,
    result: impl Fn(&T) -> Result<Value, MethodError>,
) -> Result<Value, MethodError> {
    search::requested_attachments(request.value("attachments"), &[])?;
    let order = Order::parse(request.value("order"), query_class, false)?;
    let paging = Paging::for_search(
        request.value("limit"),
        request.value("after"),
        request.value("before"),
    )?;
    let page = Query::<T>::new(request, clauses, order)
        .page(&paging)
        .await?;
    let data: Vec<Value> = page.items.iter().map(result).collect::<Result<_, _>>()?;
    Ok(search::response(request, &data, &paging, &page, |row| {
        row.id().to_string()
    }))
}

/// `harbormaster.target.search`.
pub struct TargetSearch;

impl Method for TargetSearch {
    fn name(&self) -> &'static str {
        "harbormaster.target.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let constraints = constraints::parse(request, TARGET_CONSTRAINTS)?;
            let strict = request.strictly_typed;
            let builds = constraints::datasource(&constraints, "buildPHIDs")?;
            let mut dates = Vec::new();
            for (key, condition) in [
                ("createdStart", "target.dateCreated >= ?"),
                ("createdEnd", "target.dateCreated <= ?"),
                ("startedStart", "target.dateStarted >= ?"),
                ("startedEnd", "target.dateStarted <= ?"),
                ("completedStart", "target.dateCompleted >= ?"),
                ("completedEnd", "target.dateCompleted <= ?"),
            ] {
                if let Some(epoch) = constraints::date(&constraints, key, strict)? {
                    dates.push((condition, epoch));
                }
            }
            let statuses = constraints::string_list(&constraints, "statuses")?;
            let ids = Ids::parse(&constraints, strict)?;
            let mut clauses = Clauses::default();
            ids.add_conditions(&mut clauses, "target");
            constraints::in_list(&mut clauses, "target.buildPHID", builds.as_ref());
            for (condition, epoch) in dates {
                clauses.condition(condition, [json!(epoch)]);
            }
            constraints::in_list(&mut clauses, "target.targetStatus", statuses.as_ref());
            respond(
                request,
                clauses,
                "HarbormasterBuildTargetQuery",
                |target: &Target| {
                    let policies = storage::attached(target.policies.as_ref())?;
                    let mut fields = Map::new();
                    fields.insert("name".to_owned(), json!(target_name(target)));
                    fields.insert("buildPHID".to_owned(), json!(target.build_phid.as_str()));
                    fields.insert("buildStepPHID".to_owned(), json!(target.step_phid));
                    fields.insert(
                        "status".to_owned(),
                        json!({"value": target.status, "name": status_name(&target.status)}),
                    );
                    fields.insert("epochStarted".to_owned(), epoch(target.started));
                    fields.insert("epochCompleted".to_owned(), epoch(target.completed));
                    fields.insert("buildGeneration".to_owned(), json!(target.generation));
                    Ok(crate::search_result(
                        target.id,
                        target.phid.as_str(),
                        fields,
                        target.dates,
                        policies.view_only(),
                        Map::new(),
                    ))
                },
            )
            .await
        })
    }
}

/// The constraints of the artifact and log searches: their targets, the
/// IDs and PHIDs.
fn by_target(
    request: &ApiRequest<'_>,
    alias: &str,
    allow_phids: bool,
) -> Result<Clauses, MethodError> {
    let constraints = constraints::parse(request, BY_TARGET_CONSTRAINTS)?;
    let targets = constraints::phid_list(&constraints, "buildTargetPHIDs")?;
    let ids = Ids::parse(&constraints, request.strictly_typed)?;
    if ids.phids.is_some() && !allow_phids {
        return Err(MethodError::Unsupported("the PHIDs of artifacts"));
    }
    let mut clauses = Clauses::default();
    ids.add_conditions(&mut clauses, alias);
    constraints::in_list(
        &mut clauses,
        &format!("{alias}.buildTargetPHID"),
        targets.as_ref(),
    );
    Ok(clauses)
}

/// `harbormaster.artifact.search`.
pub struct ArtifactSearch;

impl Method for ArtifactSearch {
    fn name(&self) -> &'static str {
        "harbormaster.artifact.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let clauses = by_target(request, Artifact::ALIAS, false)?;
            respond(
                request,
                clauses,
                Artifact::QUERY_CLASS,
                |artifact: &Artifact| {
                    let policies = storage::attached(artifact.policies.as_ref())?;
                    let mut fields = Map::new();
                    fields.insert(
                        "buildTargetPHID".to_owned(),
                        json!(artifact.target_phid.as_str()),
                    );
                    fields.insert("artifactType".to_owned(), json!(artifact.kind));
                    fields.insert("artifactKey".to_owned(), json!(artifact.key));
                    fields.insert("isReleased".to_owned(), json!(artifact.is_released));
                    Ok(crate::search_result(
                        artifact.id,
                        artifact.phid.as_str(),
                        fields,
                        artifact.dates,
                        policies.view_only(),
                        Map::new(),
                    ))
                },
            )
            .await
        })
    }
}

/// `harbormaster.log.search`.
pub struct LogSearch;

impl Method for LogSearch {
    fn name(&self) -> &'static str {
        "harbormaster.log.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let clauses = by_target(request, Log::ALIAS, true)?;
            respond(request, clauses, Log::QUERY_CLASS, |log: &Log| {
                let policies = storage::attached(log.policies.as_ref())?;
                let mut fields = Map::new();
                fields.insert(
                    "buildTargetPHID".to_owned(),
                    json!(log.target_phid.as_str()),
                );
                fields.insert("byteLength".to_owned(), json!(log.byte_length));
                fields.insert("filePHID".to_owned(), json!(log.file_phid));
                Ok(crate::search_result(
                    log.id,
                    log.phid.as_str(),
                    fields,
                    log.dates,
                    policies.view_only(),
                    Map::new(),
                ))
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use phab_core::Phid;

    use super::*;

    fn target(class_name: &str, name: Option<&str>) -> Target {
        Target {
            id: 1,
            phid: Phid::new("PHID-HMBT-1"),
            build_phid: Phid::new("PHID-HMBD-1"),
            step_phid: "PHID-HMCS-1".to_owned(),
            class_name: class_name.to_owned(),
            details: serde_json::Map::new(),
            variables: serde_json::Map::new(),
            name: name.map(str::to_owned),
            status: "target/passed".to_owned(),
            started: None,
            completed: None,
            generation: 1,
            dates: (1, 1),
            policies: None,
        }
    }

    #[test]
    fn names_like_php() {
        let lint = "HarbormasterArcLintBuildStepImplementation";
        let http = "HarbormasterHTTPRequestBuildStepImplementation";
        assert_eq!(
            target_name(&target(lint, Some("arcanist.lint"))),
            "Arcanist Lint Results"
        );
        assert_eq!(target_name(&target(http, Some("Ping"))), "Ping");
        assert_eq!(target_name(&target(http, None)), "Make HTTP Request");
        assert_eq!(target_name(&target("Gone", Some(""))), "Gone");
        assert_eq!(target_name(&target("Gone", Some("Kept"))), "Kept");
    }

    #[test]
    fn epochs() {
        assert_eq!(epoch(Some(0)), Value::Null);
        assert_eq!(epoch(Some(5)), json!(5));
        assert_eq!(epoch(None), Value::Null);
    }
}
