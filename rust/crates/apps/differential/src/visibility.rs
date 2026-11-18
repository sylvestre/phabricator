//! `differential.custom.revision.visibility` (moz-extensions
//! `DifferentialCustomRevisionVisibilityConduitAPIMethod`): whether a
//! revision is still waiting on phab-bot ("unprocessed"), "secure" or
//! "public", for revisions the caller may not see.
//!
//! Everything is read as the omnipotent user, and nothing about the
//! revision is returned but its ID and that state.

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_db::{Database, rows};
use serde_json::{Value, json};

/// `SECURE_PROJECT_NAME`.
const SECURE_PROJECT_NAME: &str = "secure-revision";

/// The project rules phab-bot uses interchangeably.
const PROJECT_RULES: &[&str] = &[
    "PhabricatorProjectsPolicyRule",
    "PhabricatorProjectsAllPolicyRule",
];

/// `(int)$revisionID`, for the values Rust reads like PHP; `None` is 0.
fn revision_id(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Ok(None),
        Some(Value::Number(number)) if !number.is_f64() => number
            .as_i64()
            .map(|id| (id != 0).then_some(id))
            .ok_or(MethodError::Unsupported("a huge revision ID")),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) if text.bytes().all(|b| b.is_ascii_digit()) => text
            .parse::<i64>()
            .map(|id| (id != 0).then_some(id))
            .map_err(|_| MethodError::Unsupported("a huge revision ID")),
        Some(_) => Err(MethodError::Unsupported("an unusual revision ID")),
    }
}

/// `filterPolicyProjectPHIDs()`: the projects the ALLOW project rules of
/// a custom policy grant access to.
fn policy_projects(rules: &Value) -> Vec<String> {
    let rules: Vec<&Value> = match rules {
        Value::Array(list) => list.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => return Vec::new(),
    };
    let mut phids = Vec::new();
    for rule in rules {
        let rule = match rule {
            Value::Object(rule) => rule,
            // A list is an array to PHP too.
            Value::Array(_) => continue,
            // A malformed rule grants nothing at all.
            _ => return Vec::new(),
        };
        if rule.get("action").and_then(Value::as_str) != Some("allow") {
            continue;
        }
        if !rule
            .get("rule")
            .and_then(Value::as_str)
            .is_some_and(|class| PROJECT_RULES.contains(&class))
        {
            continue;
        }
        // (array) of the value.
        match rule.get("value") {
            Some(Value::Array(values)) => {
                phids.extend(values.iter().filter_map(|v| v.as_str().map(str::to_owned)));
            }
            Some(Value::Object(values)) => {
                phids.extend(
                    values
                        .values()
                        .filter_map(|v| v.as_str().map(str::to_owned)),
                );
            }
            Some(Value::String(phid)) => phids.push(phid.clone()),
            // Null is no value; other scalars are no PHID.
            _ => {}
        }
    }
    phids
}

/// `loadPolicyProjectPHIDs()`: the projects of a custom view policy.
async fn load_policy_projects(
    database: &Database,
    view_policy: &str,
) -> Result<Vec<String>, MethodError> {
    if !view_policy.starts_with("PHID-PLCY-") {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT rules FROM {} WHERE phid = ?",
        database.table("policy", "policy")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(view_policy)
        .fetch_optional(database.pool())
        .await?
    else {
        return Ok(Vec::new());
    };
    Ok(policy_projects(&rows::json(&row, "rules")?))
}

/// `loadSecureProjectPHID()`: the #secure-revision project, by name.
async fn load_secure_project(database: &Database) -> Result<Option<String>, MethodError> {
    let sql = format!(
        "SELECT phid FROM {} WHERE name = ? LIMIT 2",
        database.table("project", "project")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(SECURE_PROJECT_NAME)
        .fetch_all(database.pool())
        .await?;
    match found.as_slice() {
        [] => Ok(None),
        [row] => Ok(Some(rows::text(row, "phid")?)),
        // executeOne() throws.
        _ => Err(MethodError::Unsupported("several secure projects")),
    }
}

/// `classifyVisibility()`.
fn classify(view_policy: &str, policy_projects: &[String], secure: Option<&str>) -> &'static str {
    if view_policy == "public" || view_policy == "users" {
        return "public";
    }
    if secure.is_some_and(|secure| policy_projects.iter().any(|phid| phid == secure)) {
        return "unprocessed";
    }
    "secure"
}

/// `differential.custom.revision.visibility`.
pub struct RevisionVisibility;

impl Method for RevisionVisibility {
    fn name(&self) -> &'static str {
        "differential.custom.revision.visibility"
    }

    fn params(&self) -> &'static [&'static str] {
        &["revisionID"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(id) = revision_id(request.value("revisionID"))? else {
                return Err(MethodError::Conduit {
                    code: "ERR-INVALID-PARAMETER".to_owned(),
                    info: Some("Parameter \"revisionID\" is required.".to_owned()),
                });
            };
            let database = &request.context.database;
            let sql = format!(
                "SELECT id, viewPolicy FROM {} WHERE id = ?",
                database.table("differential", "differential_revision")
            );
            let row = phab_db::sqlx::query(&sql)
                .bind(id)
                .fetch_optional(database.pool())
                .await?
                .ok_or_else(|| MethodError::Conduit {
                    code: "ERR-REVISION-NOT-FOUND".to_owned(),
                    info: Some(format!("Revision D{id} not found.")),
                })?;
            let view_policy = rows::text(&row, "viewPolicy")?;
            let projects = load_policy_projects(database, &view_policy).await?;
            let secure = load_secure_project(database).await?;
            Ok(json!({
                // Lisk keeps MySQL's strings.
                "revisionID": rows::int(&row, "id")?.to_string(),
                "visibility": classify(&view_policy, &projects, secure.as_deref()),
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_allowed_project_rules() {
        let rules = json!([
            {"action": "allow", "rule": "PhabricatorProjectsPolicyRule", "value": ["PHID-PROJ-a"]},
            {"action": "deny", "rule": "PhabricatorProjectsPolicyRule", "value": ["PHID-PROJ-b"]},
            {"action": "allow", "rule": "PhabricatorUsersPolicyRule", "value": ["PHID-USER-c"]},
            {"action": "allow", "rule": "PhabricatorProjectsAllPolicyRule", "value": "PHID-PROJ-d"},
        ]);
        assert_eq!(policy_projects(&rules), ["PHID-PROJ-a", "PHID-PROJ-d"]);
        assert_eq!(
            policy_projects(&json!([{"action": "allow"}, "bad"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn classifies_like_php() {
        let secure = Some("PHID-PROJ-s");
        assert_eq!(classify("users", &[], secure), "public");
        assert_eq!(
            classify("PHID-PLCY-x", &["PHID-PROJ-s".to_owned()], secure),
            "unprocessed"
        );
        assert_eq!(classify("PHID-PLCY-x", &[], secure), "secure");
        assert_eq!(classify("admin", &[], None), "secure");
        assert_eq!(revision_id(Some(&json!("0"))), Ok(None));
        assert_eq!(revision_id(Some(&json!("12"))), Ok(Some(12)));
    }
}
