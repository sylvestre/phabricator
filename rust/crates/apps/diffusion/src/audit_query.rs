//! `audit.query` (`AuditQueryConduitAPIMethod`): the audit requests of the
//! commits `DiffusionCommitQuery` finds by auditor, commit and status,
//! with an offset and a limit on commits.
//!
//! Ported: every parameter. Left to PHP: values of unusual types
//! (lists which are not lists of strings, statuses which are arrays,
//! negative or non-integer offsets and limits).

use std::collections::{HashMap, HashSet};

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, pager};
use phab_core::php;
use phab_db::{Database, rows};
use serde_json::{Value, json};

use crate::commit_search::{CommitRow, Commits, Filters, Order};

/// An `optional list<phid>` parameter: falsy is no constraint.
fn phid_list(request: &ApiRequest<'_>, key: &str) -> Result<Option<Vec<String>>, MethodError> {
    match request.value(key) {
        Some(Value::Array(list)) if !list.is_empty() => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        Some(value) if php::is_truthy(value) => {
            Err(MethodError::Unsupported("lists which are not lists"))
        }
        _ => Ok(None),
    }
}

/// The `status` parameter: the audit statuses of its commits
/// (`$status_map`); unknown statuses and `audit-status-any` are none.
fn statuses(value: Option<&Value>) -> Result<Option<Vec<String>>, MethodError> {
    let status = match value {
        Some(Value::Array(_) | Value::Object(_)) => {
            return Err(MethodError::Unsupported("a status which is not a string"));
        }
        Some(Value::String(status)) => status.as_str(),
        // Other scalars are never keys of the map.
        _ => return Ok(None),
    };
    let statuses: &[&str] = match status {
        "audit-status-open" => &["needs-audit", "concern-raised"],
        "audit-status-concern" => &["concern-raised"],
        "audit-status-accepted" => &["audited"],
        "audit-status-partial" => &["partially-audited"],
        _ => return Ok(None),
    };
    Ok(Some(
        statuses.iter().map(|&status| status.to_owned()).collect(),
    ))
}

/// A `repository_auditrequest` row.
struct AuditRequest {
    id: String,
    commit_phid: String,
    auditor_phid: String,
    status: String,
}

/// `needAuditRequests()`: the audit requests of these commits, by commit
/// PHID, in the order of PHP's query, which has no `ORDER BY`.
async fn load_audits(
    database: &Database,
    commits: &[CommitRow],
) -> Result<HashMap<String, Vec<AuditRequest>>, MethodError> {
    let mut audits: HashMap<String, Vec<AuditRequest>> = HashMap::new();
    if commits.is_empty() {
        return Ok(audits);
    }
    // The same query as Lisk's loadAllWhere(), so MySQL picks the same
    // plan and returns the rows in the same order.
    let sql = format!(
        "SELECT * FROM {} WHERE commitPHID IN ({})",
        database.table("repository", "repository_auditrequest"),
        phab_db::placeholders(commits.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        commits.iter().map(|commit| commit.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    for row in &found {
        let audit = AuditRequest {
            // Lisk keeps MySQL's strings.
            id: rows::int(row, "id")?.to_string(),
            commit_phid: rows::text(row, "commitPHID")?,
            auditor_phid: rows::text(row, "auditorPHID")?,
            status: rows::text(row, "auditStatus")?,
        };
        audits
            .entry(audit.commit_phid.clone())
            .or_default()
            .push(audit);
    }
    Ok(audits)
}

/// `audit.query`.
pub struct AuditQuery;

impl Method for AuditQuery {
    fn name(&self) -> &'static str {
        "audit.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &["auditorPHIDs", "commitPHIDs", "status", "offset", "limit"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            // array_fuse() of the value takes only arrays.
            if matches!(
                request.value("auditorPHIDs"),
                Some(Value::Bool(_) | Value::Number(_) | Value::String(_))
            ) {
                return Err(MethodError::Unsupported("auditors which are not a list"));
            }
            let auditors = phid_list(request, "auditorPHIDs")?;
            let filters = Filters {
                auditors: auditors.clone(),
                phids: phid_list(request, "commitPHIDs")?,
                statuses: statuses(request.value("status"))?,
                ..Filters::default()
            };
            let offset = pager::offset_value(request.value("offset"), 0)?;
            let limit = pager::offset_value(request.value("limit"), 100)?;
            // getDefaultOrderVector(): by ID.
            let query = Commits::resolve(request, &filters, Order::ImportNew).await?;
            let commits = pager::execute_with_offset(&query, offset, limit).await?;
            let mut audits = load_audits(&request.context.database, &commits).await?;
            let auditor_map: Option<HashSet<String>> =
                auditors.map(|auditors| auditors.into_iter().collect());
            let mut results = Vec::new();
            for commit in &commits {
                for audit in audits.remove(commit.phid.as_str()).unwrap_or_default() {
                    if auditor_map
                        .as_ref()
                        .is_some_and(|map| !map.contains(&audit.auditor_phid))
                    {
                        continue;
                    }
                    results.push(json!({
                        "id": audit.id,
                        "commitPHID": audit.commit_phid,
                        "auditorPHID": audit.auditor_phid,
                        "reasons": [],
                        "status": audit.status,
                    }));
                }
            }
            Ok(Value::Array(results))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_legacy_statuses() {
        assert_eq!(
            statuses(Some(&json!("audit-status-open"))),
            Ok(Some(vec![
                "needs-audit".to_owned(),
                "concern-raised".to_owned()
            ]))
        );
        assert_eq!(statuses(Some(&json!("audit-status-any"))), Ok(None));
        assert_eq!(statuses(Some(&json!("nope"))), Ok(None));
        assert_eq!(statuses(Some(&json!(3))), Ok(None));
        assert_eq!(statuses(None), Ok(None));
        assert!(statuses(Some(&json!(["audit-status-open"]))).is_err());
    }
}
