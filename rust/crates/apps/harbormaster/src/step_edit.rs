//! `harbormaster.step.edit` (`HarbormasterBuildStepEditAPIMethod`): the
//! transactions of `HarbormasterBuildStepEditEngine` on an existing step,
//! applied as `HarbormasterBuildStepEditor` does (no mail, no feed, no
//! Herald).
//!
//! The engine builds no field of its own: its edit types are the custom
//! fields of the step implementation (`HarbormasterBuildStepCoreCustomField`,
//! `custom.<key>`; `PhabricatorAuthMFAEditEngineExtension`, which would add
//! `mfa`, is disabled). A field transaction (`core:customfield`)
//! sets `details[<key>]`; the field reads its value from the step, never
//! from storage, so the old value is always null, every transaction has
//! an effect, and the only validation error, a required field left empty,
//! is a missing field error, which edits drop. Ported: the text and select
//! fields of the implementations of this install, and edits without
//! transactions. Left to PHP: creations, credentials, unknown
//! implementations, and steps the viewer can't see or edit.

use phab_conduit::edit::{self, Planned, Publication, RawTransaction};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{PhidType, php};
use serde_json::{Map, Value, json};

use crate::object_edit;
use crate::storage::Step;

/// The PHID type of build steps.
const STEP: PhidType = PhidType::new(*b"HMCS");
/// `PhabricatorTransactions::TYPE_CUSTOMFIELD`.
const CUSTOM_FIELD: &str = "core:customfield";
/// `HarbormasterBuildStepCoreCustomField::getStandardCustomFieldNamespace()`.
const NAMESPACE: &str = "harbormaster:core";

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// How a custom field's Conduit edit type reads its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `text` and `select` fields: `ConduitStringParameterType`.
    String,
    /// `credential` fields, which have no Conduit parameter type.
    Credential,
}

/// The fields of `HarbormasterHTTPRequestBuildStepImplementation`
/// (`getFieldSpecifications()`, then the `builtin.wait-for-message` field
/// of the implementations which support waiting for a message; its
/// header has no edit type), by raw key.
const HTTP_REQUEST_FIELDS: &[(&str, Kind)] = &[
    ("uri", Kind::String),
    ("method", Kind::String),
    ("credential", Kind::Credential),
    ("content_type", Kind::String),
    ("http_body", Kind::String),
    ("builtin.wait-for-message", Kind::String),
];

/// The custom fields of an implementation, in order; `None` for the
/// implementations this install doesn't have.
fn fields(class: &str) -> Option<&'static [(&'static str, Kind)]> {
    match class {
        "HarbormasterHTTPRequestBuildStepImplementation" => Some(HTTP_REQUEST_FIELDS),
        "HarbormasterAbortOlderBuildsBuildStepImplementation"
        | "HarbormasterArcLintBuildStepImplementation"
        | "HarbormasterArcUnitBuildStepImplementation"
        | "HarbormasterWaitForPreviousBuildStepImplementation" => Some(&[]),
        _ => None,
    }
}

/// The edit types of the engine for a step of an implementation with
/// these fields (`getModernFieldKey()`: the fields are not builtin).
fn edit_types(fields: &[(&str, Kind)]) -> Vec<String> {
    fields
        .iter()
        .map(|(key, _)| format!("custom.{key}"))
        .collect()
}

/// The transactions of the request and the details they leave.
fn plan_changes(
    fields: &[(&str, Kind)],
    details: &Map<String, Value>,
    raw: &[RawTransaction],
) -> Result<(Vec<Planned>, Map<String, Value>), MethodError> {
    let mut details = details.clone();
    let mut transactions = Vec::new();
    for xaction in raw {
        let kind = xaction.kind.as_str();
        let Some((key, field)) = kind
            .strip_prefix("custom.")
            .and_then(|key| fields.iter().find(|(known, _)| *known == key))
        else {
            unreachable!("the types were checked");
        };
        if *field == Kind::Credential {
            return Err(unsupported("credentials of build steps"));
        }
        let value = edit::string(kind, &xaction.value)?;
        let mut planned = Planned::new(STEP, CUSTOM_FIELD, (Value::Null, json!(value)));
        planned.metadata.insert(
            "customfield:key".to_owned(),
            json!(format!("std:{NAMESPACE}:{key}")),
        );
        transactions.push(planned);
        details.insert((*key).to_owned(), json!(value));
    }
    Ok((transactions, details))
}

/// Whether the viewer can edit the step's plan, whose policies the step
/// has.
async fn can_edit(request: &ApiRequest<'_>, step: &Step) -> Result<bool, MethodError> {
    let plans = crate::web::plans_by_phid(request, std::slice::from_ref(&step.plan_phid)).await?;
    let Some(plan) = plans.get(&step.plan_phid) else {
        return Ok(false);
    };
    let can_edit = crate::web::can_edit_plans(request, std::slice::from_ref(plan)).await?;
    Ok(can_edit.first().copied().unwrap_or(false))
}

/// `harbormaster.step.edit`.
pub struct StepEdit;

impl StepEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let raw = edit::raw_transactions(request)?;
        let Some(identifier) = edit::identifier(request)? else {
            return Err(unsupported("creating build steps"));
        };
        let step: Step = object_edit::load_object(request, &identifier).await?;
        let fields =
            fields(&step.class_name).ok_or_else(|| unsupported("unknown step implementations"))?;
        let types = edit_types(fields);
        edit::check_types(&raw, &types.iter().map(String::as_str).collect::<Vec<_>>())?;
        let (transactions, details) = plan_changes(fields, &step.details, &raw)?;
        let actor = crate::writes::actor(request)?;
        // Field transactions need `CAN_EDIT`, and so do edits without
        // transactions (`applyCapabilityChecks()`).
        if !can_edit(request, &step).await? {
            return Err(unsupported("steps the viewer can't edit"));
        }

        let database = &request.context.database;
        let publication =
            Publication::new(database, actor, step.phid.as_str().to_owned(), transactions).await?;
        let now = request.now;
        let table = database.table("harbormaster", "harbormaster_buildstep");
        let mut transaction = database.pool().begin().await?;
        // Lisk saves every column; only these change, once `details`
        // encodes back as it is stored.
        object_edit::lock_row(&mut transaction, &table, step.id, &["details"]).await?;
        phab_db::sqlx::query(&format!(
            "UPDATE {table} SET details = ?, dateModified = ? WHERE id = ?"
        ))
        .bind(php::json_encode(&php::round_trip(&Value::Object(details))))
        .bind(now)
        .bind(step.id)
        .execute(&mut *transaction)
        .await?;
        let table = database.table("harbormaster", "harbormaster_buildsteptransaction");
        publication
            .write_transactions(&mut transaction, &table, now)
            .await?;
        publication
            .schedule(&mut transaction, database, now)
            .await?;
        transaction.commit().await?;
        Ok(publication.response(step.id))
    }
}

impl Method for StepEdit {
    fn name(&self) -> &'static str {
        "harbormaster.step.edit"
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
    use super::*;

    fn raw(kind: &str, value: Value) -> RawTransaction {
        RawTransaction {
            kind: kind.to_owned(),
            value,
        }
    }

    #[test]
    fn lists_edit_types_like_php() {
        assert_eq!(
            edit_types(HTTP_REQUEST_FIELDS),
            [
                "custom.uri",
                "custom.method",
                "custom.credential",
                "custom.content_type",
                "custom.http_body",
                "custom.builtin.wait-for-message",
            ]
        );
        assert_eq!(edit_types(&[]), Vec::<String>::new());
    }

    #[test]
    fn plans_field_transactions() {
        let mut details = Map::new();
        details.insert("uri".to_owned(), json!("https://a"));
        details.insert("dependsOn".to_owned(), json!([]));
        let (transactions, details) = plan_changes(
            HTTP_REQUEST_FIELDS,
            &details,
            &[
                raw("custom.uri", json!("https://b")),
                raw("custom.builtin.wait-for-message", json!("wait")),
            ],
        )
        .unwrap();
        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0].old, Value::Null);
        assert_eq!(transactions[0].new, json!("https://b"));
        assert_eq!(
            Value::Object(transactions[1].metadata.clone()),
            json!({"customfield:key": "std:harbormaster:core:builtin.wait-for-message"})
        );
        assert_eq!(
            Value::Object(details),
            json!({"uri": "https://b", "dependsOn": [], "builtin.wait-for-message": "wait"})
        );
        assert!(matches!(
            plan_changes(
                HTTP_REQUEST_FIELDS,
                &Map::new(),
                &[raw("custom.credential", json!("PHID-CDTL-1"))]
            ),
            Err(MethodError::Unsupported(_))
        ));
    }
}
