//! `policy.query` (moz-extensions `PolicyQueryConduitAPIMethod`): what
//! phab-bot reads of custom policies. `policy.create` is in [`create`].
//!
//! Ported: custom policies (`PHID-PLCY-...`), which are not policy
//! filtered. Left to PHP: global, project and user policies (named from
//! handles), and more policies than the limit (the cursor).

pub mod create;

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture, Registry};
use phab_core::php;
use phab_db::rows;
use serde_json::{Value, json};

/// `PhabricatorPolicyPHIDTypePolicy`.
const CUSTOM_PREFIX: &str = "PHID-PLCY-";

/// `ERR-INVALID-PARAMETER` with a description.
fn invalid(info: String) -> MethodError {
    MethodError::Conduit {
        code: "ERR-INVALID-PARAMETER".to_owned(),
        info: Some(info),
    }
}

/// `ConduitAPIMethod::newPager()`: the limit, 100 by default, between 1
/// and 1000.
fn page_size(value: Option<&Value>) -> Result<usize, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(100),
        Some(Value::Number(number)) if !number.is_f64() => {
            let limit = number
                .as_i64()
                .ok_or(MethodError::Unsupported("a huge limit"))?;
            usize::try_from(limit.clamp(1, 1000)).map_err(MethodError::internal)
        }
        Some(_) => Err(MethodError::Unsupported("an unusual limit")),
    }
}

/// `policy.query`.
pub struct PolicyQuery;

impl Method for PolicyQuery {
    fn name(&self) -> &'static str {
        "policy.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phids", "limit", "offset"]
    }

    /// The method belongs to no application.
    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let size = page_size(request.value("limit"))?;
            let phids: Vec<&str> = match request.value("phids") {
                Some(Value::Array(list)) if !list.is_empty() => list
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<_>>()
                    .ok_or(MethodError::Unsupported("PHIDs which are not strings"))?,
                Some(value) if php::is_truthy(value) => {
                    return Err(MethodError::Unsupported("PHID lists which are not lists"));
                }
                _ => return Err(invalid("PHIDs required".to_owned())),
            };
            if phids.iter().any(|phid| !phid.starts_with(CUSTOM_PREFIX)) {
                return Err(MethodError::Unsupported("policies other than custom ones"));
            }
            let database = &request.context.database;
            // msort() by getSortKey() keeps the rows' order: every custom
            // policy has the same key. MySQL reads them by ID.
            let sql = format!(
                "SELECT phid, rules, defaultAction FROM {} WHERE phid IN ({}) ORDER BY id",
                database.table("policy", "policy"),
                phab_db::placeholders(phids.len())
            );
            let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().copied())
                .fetch_all(database.pool())
                .await?;
            if found.is_empty() {
                return Err(invalid(format!("Unknown policies: {}", phids.join(", "))));
            }
            if found.len() > size {
                return Err(MethodError::Unsupported("more policies than the limit"));
            }
            let data = found
                .iter()
                .map(|row| {
                    Ok(json!({
                        "phid": rows::text(row, "phid")?,
                        "type": "custom",
                        "name": "Custom Policy",
                        "shortName": "Custom Policy",
                        "fullName": "Custom Policy",
                        "href": null,
                        "workflow": null,
                        "icon": "fa-certificate",
                        "default": rows::text(row, "defaultAction")?,
                        "rules": rows::json(row, "rules")?,
                    }))
                })
                .collect::<Result<Vec<_>, phab_db::DbError>>()?;
            Ok(json!({
                "data": data,
                "cursor": {"limit": size, "after": null, "before": null},
            }))
        })
    }
}

/// Registers the policy methods.
pub fn register(registry: &mut Registry) {
    registry.register(create::PolicyCreate);
    registry.register(PolicyQuery);
}
