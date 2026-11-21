//! `harbormaster.queryautotargets`
//! (`HarbormasterQueryAutotargetsConduitAPIMethod`): the autotargets of a
//! diff, which `arc diff` reports lint and unit results to.
//!
//! `HarbormasterTargetEngine::buildTargets()` creates the autoplan, its
//! steps, the buildable and the build when they are missing; only the case
//! where they all exist, which writes nothing, is ported. Left to PHP:
//! every case which would create something, objects named by monograms or
//! other than diffs, diffs the viewer can't see, and targets of unknown
//! implementations.

use std::collections::HashMap;

use phab_conduit::pager::{self, Direction, core_error};
use phab_conduit::search::Clauses;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType};
use serde_json::{Map, Value, json};

use crate::query::{Order, Query};
use crate::storage::{Build, Buildable, Plan, Step, Target};
use crate::visibility;

/// `HarbormasterArcLintBuildStepImplementation::STEPKEY`.
pub const LINT_KEY: &str = "arcanist.lint";

/// `HarbormasterArcUnitBuildStepImplementation::STEPKEY`.
pub const UNIT_KEY: &str = "arcanist.unit";

/// `HarbormasterBuildArcanistAutoplan::PLANKEY`, the plan of both.
const PLAN_KEY: &str = "arcanist";

/// Something `buildTargets()` would create.
const CREATES: MethodError = MethodError::Unsupported("autotargets which must be created");

/// `PhabricatorObjectQuery::withNames(array($phid))->executeOne()` for a
/// diff PHID: `Ok(None)` when it doesn't exist.
async fn load_diff(request: &ApiRequest<'_>, phid: &str) -> Result<Option<Phid>, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE phid = ?",
        database.table("differential", "differential_diff")
    );
    if phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let phid = Phid::new(phid);
    if !visibility::buildable_objects(request, std::slice::from_ref(&phid))
        .await?
        .contains_key(&phid)
    {
        return Err(MethodError::Unsupported("a diff the viewer can't see"));
    }
    Ok(Some(phid))
}

/// The `targetKeys` parameter, which defaults to an empty list.
fn target_keys(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(keys)) => keys
            .iter()
            .map(|key| key.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or(MethodError::Unsupported(
                "target keys which are not strings",
            )),
        Some(_) => Err(MethodError::Unsupported("target keys which are not a list")),
    }
}

/// `getAutosteps()`: every key must be an autotarget step key.
fn check_keys(keys: &[String]) -> Result<(), MethodError> {
    for key in keys {
        match key.as_str() {
            LINT_KEY | UNIT_KEY => {}
            // mpull() files the implementations without a key under "".
            "" => return Err(MethodError::Unsupported("an empty autotarget key")),
            other => {
                return Err(core_error(format!(
                    "No build step provides autotarget \"{other}\"!"
                )));
            }
        }
    }
    Ok(())
}

/// `generateBuildStepMap()` when the plan and the steps of `keys` exist:
/// the plan.
async fn autoplan(request: &ApiRequest<'_>, keys: &[String]) -> Result<Plan, MethodError> {
    let mut clauses = Clauses::default();
    clauses.condition("plan.planAutoKey = ?", [json!(PLAN_KEY)]);
    let plan = Query::<Plan>::new(request, clauses, Order::Newest)
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or(CREATES)?;
    let mut clauses = Clauses::default();
    clauses.condition("step.buildPlanPHID = ?", [json!(plan.phid.as_str())]);
    let steps: Vec<Step> = Query::<Step>::new(request, clauses, Order::Newest)
        .all()
        .await?;
    if keys
        .iter()
        .any(|key| !steps.iter().any(|step| step.auto_key.as_ref() == Some(key)))
    {
        return Err(CREATES);
    }
    Ok(plan)
}

/// `HarbormasterBuildable::createOrLoadExisting()` when it exists: the
/// newest automatic buildable of the diff.
async fn buildable(request: &ApiRequest<'_>, diff: &Phid) -> Result<Buildable, MethodError> {
    let mut clauses = Clauses::default();
    clauses.condition("buildable.buildablePHID = ?", [json!(diff.as_str())]);
    clauses.condition("buildable.isManualBuildable = ?", [json!(0)]);
    let query = Query::<Buildable>::new(request, clauses, Order::Newest);
    pager::execute(&query, None, Direction::Forward, 1)
        .await?
        .into_iter()
        .next()
        .ok_or(CREATES)
}

/// `generateBuildTargetMap()` when the build exists: the targets of the
/// current generation of each build, by step key.
async fn target_map(
    request: &ApiRequest<'_>,
    buildable: &Buildable,
    plan: &Plan,
) -> Result<HashMap<String, Phid>, MethodError> {
    let mut clauses = Clauses::default();
    clauses.condition("b.buildablePHID = ?", [json!(buildable.phid.as_str())]);
    clauses.condition("b.buildPlanPHID = ?", [json!(plan.phid.as_str())]);
    let builds = Query::<Build>::new(request, clauses, Order::Newest)
        .all()
        .await?;
    if builds.is_empty() {
        return Err(CREATES);
    }
    let mut clauses = Clauses::default();
    clauses.in_list(
        "target.buildPHID",
        builds.iter().map(|build| json!(build.phid.as_str())),
    );
    // needBuildTargets(): the targets oldest first, grouped by build.
    let mut targets: HashMap<Phid, Vec<Target>> = HashMap::new();
    for target in Query::<Target>::new(request, clauses, Order::Oldest)
        .all()
        .await?
    {
        targets
            .entry(target.build_phid.clone())
            .or_default()
            .push(target);
    }
    let mut map = HashMap::new();
    for build in &builds {
        for target in targets.get(&build.phid).into_iter().flatten() {
            if target.generation != build.generation {
                continue;
            }
            let (_, key) = crate::step_implementation(&target.class_name).ok_or(
                MethodError::Unsupported("targets of unknown implementations"),
            )?;
            if let Some(key) = key {
                map.insert(key.to_owned(), target.phid.clone());
            }
        }
    }
    Ok(map)
}

/// `harbormaster.queryautotargets`.
pub struct QueryAutotargets;

impl Method for QueryAutotargets {
    fn name(&self) -> &'static str {
        "harbormaster.queryautotargets"
    }

    fn params(&self) -> &'static [&'static str] {
        &["objectPHID", "targetKeys"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let name = match request.value("objectPHID") {
                Some(Value::String(name))
                    if Phid::new(name.as_str()).phid_type() == PhidType::DIFF =>
                {
                    name
                }
                _ => return Err(MethodError::Unsupported("autotargets of other objects")),
            };
            let Some(diff) = load_diff(request, name).await? else {
                return Err(core_error(format!("No such object \"{name}\" exists.")));
            };
            let keys = target_keys(request.value("targetKeys"))?;
            if keys.is_empty() {
                return Ok(json!({"targetMap": []}));
            }
            check_keys(&keys)?;
            let plan = autoplan(request, &keys).await?;
            let buildable = buildable(request, &diff).await?;
            let targets = target_map(request, &buildable, &plan).await?;
            // array_select_keys(): in the order asked, each key once.
            let mut map = Map::new();
            for key in &keys {
                if let Some(phid) = targets.get(key) {
                    map.insert(key.clone(), json!(phid.as_str()));
                }
            }
            let map = if map.is_empty() {
                json!([])
            } else {
                Value::Object(map)
            };
            Ok(json!({"targetMap": map}))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(target_keys(None), Ok(vec![]));
        assert!(target_keys(Some(&json!("x"))).is_err());
        assert_eq!(check_keys(&[LINT_KEY.to_owned()]), Ok(()));
        assert_eq!(
            check_keys(&["x".to_owned()]),
            Err(core_error(
                "No build step provides autotarget \"x\"!".to_owned()
            ))
        );
    }
}
