//! `harbormaster.buildplan.edit` (`HarbormasterBuildPlanEditConduitAPIMethod`):
//! the transactions of `HarbormasterBuildPlanEditEngine` on an existing
//! plan, applied as `HarbormasterBuildPlanEditor` does (no mail, no feed,
//! no Herald; the publish task indexes the plan).
//!
//! Ported edit types: `name` and the plan behaviors (`behavior.<key>`),
//! once each. Left to PHP: creating plans, the other types (policies,
//! projects, subscribers), empty names, plans the viewer can't see or
//! edit (PHP raises policy exceptions), edits without effect, and several
//! validation errors at once.

use phab_conduit::edit::{self, Identifier, Planned, Publication, RawTransaction};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{PhidType, php};
use serde_json::{Value, json};

use crate::plan::BEHAVIORS;
use crate::storage::{self, Plan};

/// The PHID type of build plans.
const PLAN: PhidType = PhidType::new(*b"HMCP");
/// The byte length of the `name` column (`sort128`).
const NAME_LENGTH: usize = 128;
/// `HarbormasterBuildPlanBehavior::getTransactionMetadataKey()`.
const BEHAVIOR_KEY: &str = "behavior-key";

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// One transaction of the request.
enum Change {
    Name(String),
    /// A behavior, by its index in [`BEHAVIORS`], and the option.
    Behavior(usize, String),
}

/// The transactions of the request, as the edit types build them.
fn changes(raw: &[RawTransaction]) -> Result<Vec<Change>, MethodError> {
    edit::check_unique(
        &raw.iter()
            .map(|xaction| xaction.kind.as_str())
            .collect::<Vec<_>>(),
    )?;
    raw.iter()
        .map(|xaction| {
            let kind = xaction.kind.as_str();
            if kind == "name" {
                return Ok(Change::Name(edit::string(kind, &xaction.value)?));
            }
            let behavior = kind
                .strip_prefix("behavior.")
                .and_then(|key| BEHAVIORS.iter().position(|(known, _, _)| *known == key))
                .ok_or_else(|| unsupported("transactions not ported"))?;
            Ok(Change::Behavior(
                behavior,
                edit::string(kind, &xaction.value)?,
            ))
        })
        .collect()
}

/// `newObjectFromIdentifier()` with `CAN_VIEW`, then `CAN_EDIT`, which
/// every transaction needs (`applyCapabilityChecks()`).
async fn load_plan(request: &ApiRequest<'_>, identifier: Identifier) -> Result<Plan, MethodError> {
    let database = &request.context.database;
    let (column, value) = match identifier {
        Identifier::Id(id) => ("id", id.to_string()),
        Identifier::Phid(phid) => ("phid", phid),
    };
    let found: Vec<Plan> = storage::load_where(database, column, &[value]).await?;
    let plan = crate::visibility::visible_plans(request, found)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| unsupported("plans the viewer can't see"))?;
    let can_edit = crate::web::can_edit_plans(request, std::slice::from_ref(&plan)).await?;
    if !can_edit.first().copied().unwrap_or(false) {
        return Err(unsupported("plans the viewer can't edit"));
    }
    Ok(plan)
}

/// The values of the transactions, the plan as they leave it, and the
/// validation errors, in the order of the editor's types (the name, then
/// the behaviors).
fn plan_changes(
    plan: &Plan,
    changes: Vec<Change>,
) -> Result<(Vec<Planned>, Plan, Vec<String>), MethodError> {
    let mut edited = plan.clone();
    let mut transactions = Vec::new();
    let mut errors = Vec::new();
    for change in changes {
        match change {
            Change::Name(name) => {
                if name.is_empty() {
                    // A missing field error, which edits drop.
                    return Err(unsupported("empty names on edits"));
                }
                if name.len() > NAME_LENGTH {
                    return Err(unsupported("names longer than their column"));
                }
                transactions.push(Planned::new(
                    PLAN,
                    "harbormaster:name",
                    (json!(plan.name), json!(name)),
                ));
                edited.name = name;
            }
            Change::Behavior(index, value) => {
                let (key, options, _) = BEHAVIORS[index];
                let storage_key = format!("behavior.{key}");
                let stored = plan.properties.get(&storage_key);
                let old = crate::plan::behavior(plan, key)?;
                if stored != Some(&json!(value)) && !options.contains(&value.as_str()) {
                    errors.push(format!(
                        "Value \"{value}\" is not a valid option for behavior \"{key}\". Valid \
                         options are: {}.",
                        options.join(", ")
                    ));
                }
                let mut planned = Planned::new(PLAN, "behavior", (json!(old), json!(value)));
                planned.metadata.insert(BEHAVIOR_KEY.to_owned(), json!(key));
                transactions.push(planned);
                edited.properties.insert(storage_key, json!(value));
            }
        }
    }
    Ok((transactions, edited, errors))
}

/// `harbormaster.buildplan.edit`.
pub struct BuildPlanEdit;

impl BuildPlanEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let raw = edit::raw_transactions(request)?;
        let Some(identifier) = edit::identifier(request)? else {
            return Err(unsupported("creating build plans"));
        };
        let actor = crate::writes::actor(request)?;
        let plan = load_plan(request, identifier).await?;
        let changes = changes(&raw)?;
        let (transactions, edited, errors) = plan_changes(&plan, changes)?;
        match errors.as_slice() {
            [] => {}
            [_] => return Err(edit::validation_error(&errors)),
            _ => return Err(unsupported("several validation errors")),
        }
        let transactions: Vec<Planned> = transactions
            .into_iter()
            .filter(Planned::has_effect)
            .collect();
        if transactions.is_empty() {
            return Err(unsupported("edits without effect"));
        }

        let database = &request.context.database;
        let publication =
            Publication::new(database, actor, plan.phid.as_str().to_owned(), transactions).await?;
        let now = request.now;
        let mut transaction = database.pool().begin().await?;
        // Lisk saves every column; only these change. The properties were
        // read as a dictionary, which a list (but an empty one) is not.
        let table = database.table("harbormaster", "harbormaster_buildplan");
        let stored = phab_db::sqlx::query(&format!(
            "SELECT properties FROM {table} WHERE id = ? FOR UPDATE"
        ))
        .bind(plan.id)
        .fetch_one(&mut *transaction)
        .await?;
        let is_dictionary = match phab_db::rows::json(&stored, "properties")? {
            Value::Object(_) => true,
            Value::Array(list) => list.is_empty(),
            _ => false,
        };
        if !is_dictionary {
            return Err(unsupported("plan properties which are not a dictionary"));
        }
        phab_db::sqlx::query(&format!(
            "UPDATE {table} SET name = ?, properties = ?, dateModified = ? WHERE id = ?"
        ))
        .bind(&edited.name)
        .bind(php::json_encode(&php::round_trip(&Value::Object(
            edited.properties.clone(),
        ))))
        .bind(now)
        .bind(plan.id)
        .execute(&mut *transaction)
        .await?;
        let table = database.table("harbormaster", "harbormaster_buildplantransaction");
        publication
            .write_transactions(&mut transaction, &table, now)
            .await?;
        publication
            .schedule(&mut transaction, database, now)
            .await?;
        transaction.commit().await?;
        Ok(publication.response(plan.id))
    }
}

impl Method for BuildPlanEdit {
    fn name(&self) -> &'static str {
        "harbormaster.buildplan.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use phab_core::Phid;
    use serde_json::Map;

    use super::*;

    fn plan(properties: Map<String, Value>) -> Plan {
        Plan {
            id: 1,
            phid: Phid::new("PHID-HMCP-1"),
            name: "Build".to_owned(),
            status: "active".to_owned(),
            auto_key: None,
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            properties,
            dates: (1, 1),
        }
    }

    #[test]
    fn plans_behaviors_like_php() {
        let (transactions, edited, errors) = plan_changes(
            &plan(Map::new()),
            vec![
                Change::Behavior(0, "never".to_owned()),
                Change::Behavior(1, "always".to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(errors.len(), 0);
        assert_eq!(transactions[0].old, json!("always"));
        assert_eq!(transactions[0].new, json!("never"));
        assert_eq!(
            transactions[0].metadata,
            json!({"behavior-key": "hold-drafts"})
                .as_object()
                .unwrap()
                .clone()
        );
        assert!(!transactions[1].has_effect());
        assert_eq!(
            Value::Object(edited.properties),
            json!({"behavior.hold-drafts": "never", "behavior.arc-land": "always"})
        );

        let (_, _, errors) =
            plan_changes(&plan(Map::new()), vec![Change::Behavior(4, "x".to_owned())]).unwrap();
        assert_eq!(
            errors,
            [
                "Value \"x\" is not a valid option for behavior \"runnable\". Valid options are: \
              edit, view."
            ]
        );
        // A stored value, valid or not, may be set again.
        let mut stored = Map::new();
        stored.insert("behavior.runnable".to_owned(), json!("x"));
        let (transactions, _, errors) =
            plan_changes(&plan(stored), vec![Change::Behavior(4, "x".to_owned())]).unwrap();
        assert_eq!(errors.len(), 0);
        assert_eq!(transactions[0].old, json!("edit"));
    }
}
