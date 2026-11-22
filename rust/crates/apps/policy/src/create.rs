//! `policy.create` (moz-extensions `CreatePolicyConduitAPIMethod`): a new
//! custom policy, which Lisk saves as one `policy` row, without
//! transactions.
//!
//! Ported: policies for revisions (`DREV`), projects (`PROJ`) and users
//! (`USER`), with the rules which can apply to them. Left to PHP: other
//! object types (unknown ones get an `Aphront404Response`, which Conduit
//! doesn't expect), values PHP checks with `PhutilTypeSpec` (whose
//! exceptions it words), values with floats, which PHP encodes its own
//! way, and default actions the column refuses.

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php};
use phab_policy::loader::{self, NoExtendedObjects};
use serde_json::{Value, json};

/// How `getValueForStorage()` stores a rule's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    /// `PhutilTypeSpec` checks a `list<string>`, `array_values()` stores it,
    /// and an empty list has no effect.
    Strings,
    /// The value as given, which always has an effect.
    AsGiven,
}

/// The concrete `PhabricatorPolicyRule` classes, their storage and the
/// object types `canApplyToObject()` accepts (`None` for every object).
const RULES: &[(&str, Storage, Option<&[&str]>)] = &[
    ("PhabricatorUsersPolicyRule", Storage::Strings, None),
    (
        "PhabricatorAdministratorsPolicyRule",
        Storage::AsGiven,
        None,
    ),
    ("PhabricatorProjectsPolicyRule", Storage::Strings, None),
    ("PhabricatorProjectsAllPolicyRule", Storage::Strings, None),
    ("PhabricatorLunarPhasePolicyRule", Storage::AsGiven, None),
    // `PhabricatorSubscribableInterface`.
    (
        "PhabricatorSubscriptionsSubscribersPolicyRule",
        Storage::AsGiven,
        Some(&["DREV"]),
    ),
    (
        "PhabricatorProjectMembersPolicyRule",
        Storage::AsGiven,
        Some(&["PROJ"]),
    ),
    (
        "PhabricatorDifferentialReviewersPolicyRule",
        Storage::AsGiven,
        Some(&["DREV"]),
    ),
    (
        "PhabricatorCalendarEventHostPolicyRule",
        Storage::AsGiven,
        Some(&["CEVT"]),
    ),
    (
        "PhabricatorCalendarEventInviteesPolicyRule",
        Storage::AsGiven,
        Some(&["CEVT"]),
    ),
    (
        "PassphraseCredentialAuthorPolicyRule",
        Storage::AsGiven,
        Some(&["CDTL"]),
    ),
];

/// The object types ported, with the application of their PHID type.
const OBJECT_TYPES: &[(&str, &str)] = &[
    ("DREV", "PhabricatorDifferentialApplication"),
    ("PROJ", "PhabricatorProjectApplication"),
    ("USER", "PhabricatorPeopleApplication"),
];

/// The byte length of the `defaultAction` column (`text32`).
const DEFAULT_ACTION_LENGTH: usize = 32;

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// `ERR-INVALID-PARAMETER` with a description.
fn invalid(info: String) -> MethodError {
    MethodError::Conduit {
        code: "ERR-INVALID-PARAMETER".to_owned(),
        info: Some(info),
    }
}

/// A value as Lisk's `phutil_json_encode()` stores it, once PHP decoded
/// it: floats and integers beyond PHP's are left to PHP.
fn storable(value: &Value) -> Result<Value, MethodError> {
    fn is_plain(value: &Value) -> bool {
        match value {
            Value::Number(number) => number.is_i64(),
            Value::Array(items) => items.iter().all(is_plain),
            Value::Object(entries) => entries.values().all(is_plain),
            _ => true,
        }
    }
    if is_plain(value) {
        Ok(php::round_trip(value))
    } else {
        Err(unsupported("values with floats"))
    }
}

/// `getValueForStorage()` and `ruleHasEffect()`.
fn store_value(storage: Storage, value: &Value) -> Result<(Value, bool), MethodError> {
    let value = storable(value)?;
    match storage {
        Storage::AsGiven => Ok((value, true)),
        Storage::Strings => match &value {
            Value::Array(items) if items.iter().all(Value::is_string) => {
                let effect = !items.is_empty();
                Ok((value, effect))
            }
            _ => Err(unsupported("rule values which are not lists of strings")),
        },
    }
}

/// `idx($rule, $key)` on a rule given as a dictionary or a list.
fn field<'a>(rule: &'a Value, key: &str) -> Result<Option<&'a Value>, MethodError> {
    match rule {
        Value::Object(entries) => Ok(entries.get(key).filter(|value| !value.is_null())),
        Value::Array(_) => Ok(None),
        // `idx()` takes arrays only.
        _ => Err(unsupported("rules which are not dictionaries")),
    }
}

/// A string field, `''` when missing.
fn text_field<'a>(rule: &'a Value, key: &str) -> Result<&'a str, MethodError> {
    match field(rule, key)? {
        None => Ok(""),
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(unsupported("rule fields which are not strings")),
    }
}

/// Whether the viewer can see the application of the object type
/// (`PhabricatorPHIDType::getAllInstalledTypes()`).
async fn can_see_application(request: &ApiRequest<'_>, class: &str) -> Result<bool, MethodError> {
    let context = request.context;
    let subject = phab_policy::ApplicationPolicies::from_config(&context.config)
        .subject(class, context.config.allow_public());
    let facts = loader::load_facts(
        &context.database,
        &context.config,
        &request.viewer,
        &[&subject],
        &NoExtendedObjects,
        request.now,
    )
    .await?;
    Ok(phab_policy::filter::can_see_application(
        &request.viewer,
        class,
        &facts,
    )?)
}

/// The rules of the request, checked and stored as PHP does, with whether
/// each has an effect.
fn rules(object_type: &str, given: &[Value]) -> Result<Vec<(Value, bool)>, MethodError> {
    let applies = |types: Option<&[&str]>| types.is_none_or(|types| types.contains(&object_type));
    let mut stored = Vec::with_capacity(given.len());
    for rule in given {
        let action = text_field(rule, "action")?;
        if action != "allow" && action != "deny" {
            return Err(invalid(format!("Invalid action '{action}'!")));
        }
        let class = text_field(rule, "rule")?;
        let Some((_, storage, _)) = RULES
            .iter()
            .find(|(name, _, types)| *name == class && applies(*types))
        else {
            return Err(invalid(format!("Invalid rule class '{class}'!")));
        };
        let value = field(rule, "value")?.unwrap_or(&Value::Null);
        let (value, effect) = store_value(*storage, value)?;
        stored.push((
            json!({"action": action, "rule": class, "value": value}),
            effect,
        ));
    }
    Ok(stored)
}

/// `policy.create`.
pub struct PolicyCreate;

impl PolicyCreate {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let object_type = match request.value("objectType") {
            Some(value) if php::is_truthy(value) => value
                .as_str()
                .ok_or_else(|| unsupported("object types which are not strings"))?,
            _ => return Err(invalid("objectType is required.".to_owned())),
        };
        let Some((_, application)) = OBJECT_TYPES
            .iter()
            .find(|(phid_type, _)| *phid_type == object_type)
        else {
            return Err(unsupported("object types not ported"));
        };
        if !can_see_application(request, application).await? {
            return Err(unsupported(
                "object types of applications the viewer can't see",
            ));
        }

        let Some(Value::Array(given)) = request.value("policy") else {
            return Err(unsupported("policies which are not lists"));
        };
        let valid: Vec<Value> = rules(object_type, given)?
            .into_iter()
            .filter(|(_, effect)| *effect)
            .map(|(rule, _)| rule)
            .collect();
        if valid.is_empty() {
            return Err(invalid("Rules do not have any effect.".to_owned()));
        }

        // `setDefaultAction()` takes anything; the column doesn't.
        let default = match request.value("default") {
            Some(Value::String(text)) if text.is_ascii() && text.len() <= DEFAULT_ACTION_LENGTH => {
                text
            }
            _ => return Err(unsupported("default actions the column refuses")),
        };
        let phid = Phid::generate(PhidType::POLICY, None).into_string();
        let database = &request.context.database;
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, rules, defaultAction, dateCreated, dateModified) VALUES (?, ?, \
             ?, ?, ?)",
            database.table("policy", "policy")
        ))
        .bind(&phid)
        .bind(php::json_encode(&Value::Array(valid)))
        .bind(default)
        .bind(request.now)
        .bind(request.now)
        .execute(database.pool())
        .await?;
        Ok(json!({
            "phid": phid,
            "name": "Custom Policy",
            "full": "Custom Policy",
            "icon": "fa-certificate",
        }))
    }
}

impl Method for PolicyCreate {
    fn name(&self) -> &'static str {
        "policy.create"
    }

    fn params(&self) -> &'static [&'static str] {
        &["objectType", "default", "policy"]
    }

    /// The method belongs to no application.
    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[("ERR-INVALID-PARAMETER", "Missing or malformed parameter.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_values_like_php() {
        assert_eq!(
            store_value(Storage::Strings, &json!(["PHID-USER-a"])).unwrap(),
            (json!(["PHID-USER-a"]), true)
        );
        assert_eq!(
            store_value(Storage::Strings, &json!({"0": "PHID-USER-a"})).unwrap(),
            (json!(["PHID-USER-a"]), true)
        );
        assert_eq!(
            store_value(Storage::Strings, &json!([])).unwrap(),
            (json!([]), false)
        );
        assert!(store_value(Storage::Strings, &json!("PHID-USER-a")).is_err());
        assert!(store_value(Storage::Strings, &json!([1])).is_err());
        assert_eq!(
            store_value(Storage::AsGiven, &json!({})).unwrap(),
            (json!([]), true)
        );
        assert!(store_value(Storage::AsGiven, &json!(1.5)).is_err());
    }

    #[test]
    fn checks_rules_in_order() {
        let rule = |action: &str, class: &str| json!({"action": action, "rule": class});
        assert_eq!(
            rules("DREV", &[rule("maybe", "x")]),
            Err(invalid("Invalid action 'maybe'!".to_owned()))
        );
        assert_eq!(
            rules("DREV", &[json!({"rule": "x"})]),
            Err(invalid("Invalid action ''!".to_owned()))
        );
        assert_eq!(
            rules(
                "USER",
                &[rule("allow", "PhabricatorDifferentialReviewersPolicyRule")]
            ),
            Err(invalid(
                "Invalid rule class 'PhabricatorDifferentialReviewersPolicyRule'!".to_owned()
            ))
        );
        assert_eq!(
            rules(
                "DREV",
                &[rule("deny", "PhabricatorDifferentialReviewersPolicyRule")]
            ),
            Ok(vec![(
                json!({
                    "action": "deny",
                    "rule": "PhabricatorDifferentialReviewersPolicyRule",
                    "value": null,
                }),
                true
            )])
        );
    }
}
