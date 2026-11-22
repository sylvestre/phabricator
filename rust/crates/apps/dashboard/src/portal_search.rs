//! `portal.search`: `PhabricatorDashboardPortalSearchEngine` through
//! `buildConduitResponse()`.
//!
//! Portals are not `PhabricatorConduitResultInterface` objects, so their
//! fields are only those of the engine extensions: `dateCreated` and
//! `dateModified` (Lisk), then `policy`. Ported: the `ids` and `phids`
//! constraints, the `newest` and `oldest` orders, paging and the
//! `projects` attachment. Left to PHP: the `query` (fulltext) and
//! `projects` constraints, the `relevance` order, saved queries and order
//! vectors.

use std::collections::HashMap;
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints, IdOrder};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::rows;
use serde_json::{Map, Value, json};

use crate::storage::{self, Portal};

const QUERY_CLASS: &str = "PhabricatorDashboardPortalQuery";

/// `getValidConstraintKeys()` of the engine's fields: the fulltext query,
/// IDs, PHIDs and projects.
const VALID_CONSTRAINTS: &[&str] = &["query", "ids", "phids", "projects"];

const ATTACHMENTS: &[&str] = &["projects"];

const ORDERS: &[&str] = &["newest", "created", "oldest", "relevance"];

/// The ported constraints.
#[derive(Debug, Default)]
struct Filters {
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
}

impl Filters {
    fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        if let Some((key, value)) = constraints.get(&["query"])
            && !search::string(key, value)?.is_empty()
        {
            return Err(MethodError::Unsupported("fulltext portal search"));
        }
        let mut filters = Self::default();
        if let Some((key, value)) = constraints.get(&["ids"]) {
            filters.ids = Some(search::int_list(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["phids"]) {
            filters.phids = Some(search::string_list(key, value, false)?);
        }
        if constraints.has(&["projects"]) {
            return Err(MethodError::Unsupported("portal project constraints"));
        }
        Ok(filters)
    }

    fn add_conditions(&self, clauses: &mut Clauses) {
        if let Some(ids) = &self.ids {
            clauses.in_list("portal.id", ids.iter().map(|id| json!(id)));
        }
        if let Some(phids) = &self.phids {
            clauses.in_list("portal.phid", search::strings(phids));
        }
    }
}

/// `PhabricatorDashboardPortalQuery` with the ported constraints.
struct Portals<'a> {
    request: &'a ApiRequest<'a>,
    filters: Filters,
    order: IdOrder,
}

impl PolicyAwareQuery for Portals<'_> {
    type Row = Portal;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &Portal) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<Portal>, MethodError> {
        let database = &self.request.context.database;
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("portal.id", ids.iter().map(|id| json!(id)));
        } else {
            self.filters.add_conditions(&mut clauses);
        }
        let (comparison, order) = search::id_paging(self.order, direction);
        if let Some(after) = after {
            clauses.condition(format!("portal.id {comparison} ?"), [json!(after)]);
        }
        let mut tail = format!(" ORDER BY portal.id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(
            &format!(
                "SELECT portal.id FROM {} portal",
                database.table("dashboard", "dashboard_portal")
            ),
            &tail,
        );
        let page: Vec<i64> = clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| rows::int(row, "id"))
            .collect::<Result<_, _>>()?;
        let mut loaded: HashMap<i64, Portal> = storage::load_portals_by_ids(database, &page)
            .await?
            .into_iter()
            .map(|portal| (portal.id, portal))
            .collect();
        Ok(page.iter().filter_map(|id| loaded.remove(id)).collect())
    }

    async fn filter(&self, rows: Vec<Portal>) -> Result<Vec<Portal>, MethodError> {
        let context = self.request.context;
        storage::visible(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)
    }
}

/// One result: the extensions' fields, then the attachments.
fn result(portal: &Portal, projects: Option<&[Phid]>) -> Value {
    let mut attachments = Map::new();
    if let Some(projects) = projects {
        attachments.insert(
            "projects".to_owned(),
            json!({"projectPHIDs": projects.iter().map(Phid::as_str).collect::<Vec<_>>()}),
        );
    }
    json!({
        "id": portal.id,
        "type": "PRTL",
        "phid": portal.phid.as_str(),
        "fields": {
            "dateCreated": portal.date_created,
            "dateModified": portal.date_modified,
            "policy": {
                "view": portal.view_policy,
                "edit": portal.edit_policy,
            },
        },
        "attachments": attachments,
    })
}

/// `portal.search`.
pub struct PortalSearch;

impl Method for PortalSearch {
    fn name(&self) -> &'static str {
        "portal.search"
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
            search::check_query_key(request)?;
            let constraints = Constraints::parse(request.value("constraints"), VALID_CONSTRAINTS)?;
            let filters = Filters::parse(&constraints, request.strictly_typed)?;
            let requested =
                search::requested_attachments(request.value("attachments"), ATTACHMENTS)?;
            let order = IdOrder::parse(request.value("order"), QUERY_CLASS, ORDERS)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let query = Portals {
                request,
                filters,
                order,
            };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let projects = if requested.is_empty() {
                HashMap::new()
            } else {
                let phids: Vec<Phid> = page
                    .items
                    .iter()
                    .map(|portal| portal.phid.clone())
                    .collect();
                storage::projects(&request.context.database, &phids).await?
            };
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|portal| {
                    let projects = (!requested.is_empty())
                        .then(|| projects.get(&portal.phid).map_or(&[][..], Vec::as_slice));
                    result(portal, projects)
                })
                .collect();
            Ok(search::response(request, &data, &paging, &page, |portal| {
                portal.id.to_string()
            }))
        })
    }
}
