//! `internal.commit.search` (`DiffusionInternalCommitSearchConduitAPIMethod`
//! with `DiffusionInternalCommitSearchEngine`): the commits of one
//! repository, with the commit refs read from its working copy.
//!
//! Ported: the check of the `repositoryPHIDs` constraint, and searches
//! which find no commits (an unknown repository, an empty one, a cursor
//! past the end). Left to PHP: any commit found (its ref comes from
//! `git log`), repositories the viewer can't see (`executeOne()` raises a
//! policy exception) or which are served by a cluster (the call is
//! proxied), the other constraints, attachments and saved queries.

use phab_app_differential::repository;
use phab_conduit::pager::{self, Paging};
use phab_conduit::search::{self, Constraints};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::rows;
use serde_json::Value;

use crate::commit_search::{Commits, Filters, Order};

/// The constraint keys of the engine's fields: its repositories, then the
/// engine extensions' fields of commits.
const VALID_CONSTRAINTS: &[&str] = &[
    "repositoryPHIDs",
    "ids",
    "phids",
    "query",
    "subscribers",
    "projects",
];

/// `newConduitCallProxyClient()`: the one repository PHID of the
/// constraints.
fn repository_phid(constraints: Option<&Value>) -> Result<String, MethodError> {
    let phids = match constraints {
        Some(Value::Object(constraints)) => constraints.get("repositoryPHIDs"),
        // A list has no "repositoryPHIDs" key.
        _ => None,
    };
    match phids {
        Some(Value::Array(phids)) if phids.len() == 1 => {
            if let Some(Value::String(phid)) = phids.first() {
                return Ok(phid.clone());
            }
        }
        // A dictionary with the keys of a list is a list to PHP.
        Some(Value::Object(phids)) if !phids.is_empty() => {
            return Err(MethodError::Unsupported(
                "repository PHIDs which are a dictionary",
            ));
        }
        _ => {}
    }
    Err(pager::core_error(
        "This internal method must be invoked with a \"repositoryPHIDs\" constraint with \
         exactly one value."
            .to_owned(),
    ))
}

/// Whether the repository with this PHID is served by an Almanac service,
/// which PHP proxies the call to.
async fn is_clustered(request: &ApiRequest<'_>, phid: &str) -> Result<bool, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT almanacServicePHID FROM {} WHERE phid = ?",
        database.table("repository", "repository")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?;
    Ok(match row {
        Some(row) => {
            rows::opt_text(&row, "almanacServicePHID")?.is_some_and(|phid| !phid.is_empty())
        }
        None => false,
    })
}

/// `internal.commit.search`.
pub struct InternalCommitSearch;

impl Method for InternalCommitSearch {
    fn name(&self) -> &'static str {
        "internal.commit.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "queryKey",
            "constraints",
            "attachments",
            "order",
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
            let phid = repository_phid(request.value("constraints"))?;
            let context = request.context;
            let database = &context.database;
            let found = repository::load(database, &[Phid::new(phid.clone())]).await?;
            if !found.is_empty() {
                let visible = repository::visible(
                    database,
                    &context.config,
                    &request.viewer,
                    found,
                    request.now,
                )
                .await
                .map_err(MethodError::internal)?;
                if visible.is_empty() {
                    return Err(MethodError::Unsupported(
                        "a repository the viewer can't see",
                    ));
                }
                if is_clustered(request, &phid).await? {
                    return Err(MethodError::Unsupported("calls proxied to a cluster"));
                }
            }

            // buildConduitResponse().
            search::check_query_key(request)?;
            let constraints = Constraints::parse(request.value("constraints"), VALID_CONSTRAINTS)?;
            if VALID_CONSTRAINTS[1..]
                .iter()
                .any(|key| constraints.has(&[key]))
            {
                return Err(MethodError::Unsupported("a commit constraint not ported"));
            }
            // Usernames and functions are resolved by the datasource.
            if !phid.starts_with("PHID-") {
                return Err(MethodError::Unsupported(
                    "constraint values which are not PHIDs",
                ));
            }
            let attachments = match request.value("attachments") {
                None | Some(Value::Null) => true,
                Some(Value::Array(list)) => list.is_empty(),
                Some(Value::Object(map)) => map.is_empty(),
                Some(_) => false,
            };
            if !attachments {
                return Err(MethodError::Unsupported("attachments"));
            }
            let order = Order::parse(request.value("order"))?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let (start, direction) = paging.start();
            let filters = Filters {
                repositories: Some(vec![phid]),
                ..Filters::default()
            };
            let query = Commits::resolve(request, &filters, order).await?;
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            if !page.items.is_empty() {
                return Err(MethodError::Unsupported(
                    "commits, whose refs are read from the working copy",
                ));
            }
            Ok(search::response(request, &[], &paging, &page, |commit| {
                commit.id.to_string()
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn needs_one_repository() {
        assert_eq!(
            repository_phid(Some(&json!({"repositoryPHIDs": ["PHID-REPO-a"]}))),
            Ok("PHID-REPO-a".to_owned())
        );
        for constraints in [
            None,
            Some(json!("x")),
            Some(json!(["PHID-REPO-a"])),
            Some(json!({})),
            Some(json!({"repositoryPHIDs": []})),
            Some(json!({"repositoryPHIDs": ["a", "b"]})),
            Some(json!({"repositoryPHIDs": [1]})),
            Some(json!({"repositoryPHIDs": "PHID-REPO-a"})),
        ] {
            assert!(
                matches!(
                    repository_phid(constraints.as_ref()),
                    Err(MethodError::Conduit { .. })
                ),
                "{constraints:?}"
            );
        }
    }
}
