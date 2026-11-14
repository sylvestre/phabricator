//! `differential.revision.search`: `DifferentialRevisionSearchEngine`
//! through `PhabricatorApplicationSearchEngine::buildConduitResponse()`.
//!
//! Ported: the `ids` and `phids` constraints, the default order (`newest`),
//! paging, and every field. Other constraints, orders, saved queries and
//! attachments are left to PHP (`MethodError::Unsupported`).

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::storage::{self, Revision};

/// `PhabricatorDifferentialApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorDifferentialApplication",
    name: "Differential",
};

const QUERY_CLASS: &str = "DifferentialRevisionQuery";
const DEFAULT_LIMIT: usize = 100;

/// The constraint keys of the engine's search fields, as PHP lists them.
const VALID_CONSTRAINTS: &[&str] = &[
    "responsiblePHIDs",
    "authorPHIDs",
    "reviewerPHIDs",
    "repositoryPHIDs",
    "statuses",
    "createdStart",
    "createdEnd",
    "modifiedStart",
    "modifiedEnd",
    "affectedPaths",
    "query",
    "ids",
    "phids",
    "subscribers",
    "projects",
];

/// The keys of `getConduitSearchAttachments()`.
const ATTACHMENTS: &[&str] = &["reviewers", "subscribers", "projects", "reviewers-extra"];

/// The custom field keys of `DifferentialBugzillaBugIDField` and
/// `DifferentialUpliftRequestCustomField` (moz-extensions).
const BUG_FIELD: &str = "differential:bugzilla-bug-id";
const UPLIFT_FIELD: &str = "differential:uplift-request";

/// The ported constraints.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Constraints {
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
}

impl Constraints {
    /// `constraints`: unknown keys are an error, as in PHP; known keys
    /// which are not ported go to PHP.
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let entries: Vec<(String, &Value)> = match value {
            None => return Ok(Self::default()),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, value)| (key.clone(), value))
                .collect(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
            Some(_) => return Err(MethodError::Unsupported("constraints which are not a map")),
        };
        if let Some((key, _)) = entries
            .iter()
            .find(|(key, _)| !VALID_CONSTRAINTS.contains(&key.as_str()))
        {
            return Err(pager::core_error(format!(
                "Constraint \"{key}\" is not a valid constraint for this query."
            )));
        }
        let mut constraints = Self::default();
        for (key, value) in entries {
            match key.as_str() {
                "ids" => {
                    let ids = nonempty_list(&key, value)?
                        .iter()
                        .enumerate()
                        .map(|(index, id)| {
                            id.as_i64().filter(|_| !id.is_f64()).ok_or_else(|| {
                                pager::core_error(format!(
                                    "Error while reading \"ids[{index}]\": Expected integer, \
                                     got something else."
                                ))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    constraints.ids = Some(ids);
                }
                "phids" => {
                    let phids = nonempty_list(&key, value)?
                        .iter()
                        .map(|phid| phid.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                        .ok_or(MethodError::Unsupported("phids which are not strings"))?;
                    constraints.phids = Some(phids);
                }
                _ => {
                    return Err(MethodError::Unsupported(
                        "a revision search constraint not ported",
                    ));
                }
            }
        }
        Ok(constraints)
    }
}

/// The value of a list constraint (`ConduitListParameterType`), with
/// PHP's errors for what is not a nonempty list.
fn nonempty_list<'v>(key: &str, value: &'v Value) -> Result<&'v Vec<Value>, MethodError> {
    let error = |what: &str| {
        Err(pager::core_error(format!(
            "Error while reading \"{key}\": Expected {what}."
        )))
    };
    match value {
        Value::Array(list) if list.is_empty() => {
            error("a nonempty list, but value is an empty list")
        }
        Value::Array(list) => Ok(list),
        // PHP decodes {"0": ...} into a list.
        Value::Object(map)
            if map
                .keys()
                .all(|key| key.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Err(MethodError::Unsupported("constraint lists sent as objects"))
        }
        Value::Object(_) => error("a list, but value is an object"),
        _ => error("a list, but value is not a list"),
    }
}

/// The other parameters, which only take the values ported.
fn check_unported(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    match request.value("queryKey") {
        None | Some(Value::Null) => {}
        Some(Value::String(key)) if key.is_empty() => {}
        Some(_) => return Err(MethodError::Unsupported("saved revision queries")),
    }
    match request.value("order") {
        None | Some(Value::Null) => {}
        Some(Value::String(order)) if order == "newest" => {}
        Some(_) => return Err(MethodError::Unsupported("revision search orders")),
    }
    match request.value("attachments") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) if list.is_empty() => {}
        // array_select_keys() ignores the keys which are not attachments.
        Some(Value::Object(map)) if !map.keys().any(|key| ATTACHMENTS.contains(&key.as_str())) => {}
        Some(_) => return Err(MethodError::Unsupported("revision search attachments")),
    }
    Ok(())
}

/// `DifferentialRevisionQuery` with the ported constraints, newest first.
struct Revisions<'a> {
    request: &'a ApiRequest<'a>,
    constraints: Constraints,
    /// What the policy filter keeps between pages; the cursor's own query
    /// has a filter of its own.
    memory: std::sync::Mutex<phab_policy::QueryMemory>,
}

impl PolicyAwareQuery for Revisions<'_> {
    type Row = Revision;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &Revision) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<Revision>, MethodError> {
        let database = &self.request.context.database;
        let mut conditions = Vec::new();
        let mut binds: Vec<Value> = Vec::new();
        // A cursor's own query has none of the constraints.
        let constraints = if ids.is_some() {
            Constraints::default()
        } else {
            self.constraints.clone()
        };
        let mut in_list = |column: &str, values: Vec<Value>| {
            conditions.push(format!(
                "{column} IN ({})",
                phab_db::placeholders(values.len())
            ));
            binds.extend(values);
        };
        if let Some(ids) = ids {
            in_list("id", ids.iter().map(|id| json!(id)).collect());
        }
        if let Some(ids) = &constraints.ids {
            in_list("id", ids.iter().map(|id| json!(id)).collect());
        }
        if let Some(phids) = &constraints.phids {
            in_list("phid", phids.iter().map(|phid| json!(phid)).collect());
        }
        // "newest": by decreasing id.
        let (comparison, order) = match direction {
            Direction::Forward => ("<", "DESC"),
            Direction::Backward => (">", "ASC"),
        };
        if let Some(after) = after {
            conditions.push(format!("id {comparison} ?"));
            binds.push(json!(after));
        }
        let mut sql = format!(
            "SELECT id FROM {}",
            database.table("differential", "differential_revision")
        );
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        let _ = write!(sql, " ORDER BY id {order}");
        if let Some(limit) = limit {
            let _ = write!(sql, " LIMIT {limit}");
        }
        let mut query = phab_db::sqlx::query(&sql);
        for bind in &binds {
            query = match bind {
                Value::String(text) => query.bind(text.as_str()),
                other => query.bind(other.as_i64()),
            };
        }
        let rows = query.fetch_all(database.pool()).await?;
        let page: Vec<i64> = rows
            .iter()
            .map(|row| rows::int(row, "id"))
            .collect::<Result<_, _>>()?;
        let mut loaded: HashMap<i64, Revision> = storage::load_revisions_by_id(database, &page)
            .await?
            .into_iter()
            .map(|revision| (revision.id, revision))
            .collect();
        Ok(page.iter().filter_map(|id| loaded.remove(id)).collect())
    }

    async fn filter_cursor(&self, rows: Vec<Revision>) -> Result<Vec<Revision>, MethodError> {
        let context = self.request.context;
        storage::visible_revisions(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)
    }

    async fn filter(&self, rows: Vec<Revision>) -> Result<Vec<Revision>, MethodError> {
        let context = self.request.context;
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| MethodError::internal("policy memory poisoned"))?
            .clone();
        let visible = storage::visible_revisions_in_query(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
            &mut memory,
        )
        .await
        .map_err(MethodError::internal)?;
        *self
            .memory
            .lock()
            .map_err(|_| MethodError::internal("policy memory poisoned"))? = memory;
        Ok(visible)
    }
}

/// What the result fields need beyond [`Revision`].
struct Extra {
    summary: String,
    test_plan: String,
    date_created: i64,
    properties: Value,
}

async fn load_extra(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<i64, Extra>, MethodError> {
    if revisions.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, summary, testPlan, dateCreated, properties FROM {} WHERE id IN ({})",
        database.table("differential", "differential_revision"),
        phab_db::placeholders(revisions.len())
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.id),
    )
    .fetch_all(database.pool())
    .await?;
    rows.iter()
        .map(|row| {
            let properties = rows::text(row, "properties")?;
            Ok((
                rows::int(row, "id")?,
                Extra {
                    summary: rows::text(row, "summary")?,
                    test_plan: rows::text(row, "testPlan")?,
                    date_created: rows::int(row, "dateCreated")?,
                    properties: serde_json::from_str(&properties).unwrap_or(Value::Null),
                },
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// The IDs of the active diffs, by PHID, as strings: PHP returns the
/// column as it reads it.
async fn load_diff_ids(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<Phid, String>, MethodError> {
    let phids: Vec<&str> = revisions
        .iter()
        .filter_map(|revision| revision.active_diff_phid.as_ref())
        .map(Phid::as_str)
        .collect();
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, phid FROM {} WHERE phid IN ({})",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(phids.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "phid")?),
                rows::int(row, "id")?.to_string(),
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// The stored values of the two custom fields, by revision PHID.
async fn load_custom_fields(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<(Phid, &'static str), String>, MethodError> {
    if revisions.is_empty() {
        return Ok(HashMap::new());
    }
    let indexes: Vec<(String, &'static str)> = [BUG_FIELD, UPLIFT_FIELD]
        .into_iter()
        .map(|key| (phab_core::hash::digest_for_index(key.as_bytes()), key))
        .collect();
    let sql = format!(
        "SELECT objectPHID, fieldIndex, fieldValue FROM {} \
         WHERE objectPHID IN ({}) AND fieldIndex IN (?, ?)",
        database.table("differential", "differential_customfieldstorage"),
        phab_db::placeholders(revisions.len())
    );
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.phid.as_str()),
    );
    let query = phab_db::bind_all(query, indexes.iter().map(|(index, _)| index.as_str()));
    let rows = query.fetch_all(database.pool()).await?;
    let mut values = HashMap::new();
    for row in &rows {
        let index = rows::text(row, "fieldIndex")?;
        if let Some((_, key)) = indexes.iter().find(|(known, _)| *known == index) {
            values.insert(
                (Phid::new(rows::text(row, "objectPHID")?), *key),
                rows::text(row, "fieldValue")?,
            );
        }
    }
    Ok(values)
}

/// The walk of a revision graph from one seed.
struct Walk {
    /// The nodes reached, in the order they were loaded.
    order: Vec<Phid>,
    reached: HashSet<Phid>,
    /// The nodes to load next.
    level: Vec<Phid>,
}

/// `stackGraph`: `DifferentialRevisionGraph` loaded whole from each
/// revision, as the "depends on" edges of every revision it reaches.
/// Edges are not filtered by policy, as in PHP.
async fn load_stack_graphs(
    database: &Database,
    seeds: &[Phid],
) -> Result<HashMap<Phid, Map<String, Value>>, MethodError> {
    // The edges of each node, both ways, newest first.
    let mut known: HashMap<Phid, (Vec<Phid>, Vec<Phid>)> = HashMap::new();
    // AbstractDirectedGraph::loadGraph(), from every seed at once: each
    // keeps its own order of nodes, loaded level by level.
    let mut walks: Vec<Walk> = seeds
        .iter()
        .map(|seed| Walk {
            order: Vec::new(),
            reached: std::iter::once(seed.clone()).collect(),
            level: vec![seed.clone()],
        })
        .collect();
    while walks.iter().any(|walk| !walk.level.is_empty()) {
        let mut missing: Vec<Phid> = walks
            .iter()
            .flat_map(|walk| walk.level.iter())
            .filter(|node| !known.contains_key(*node))
            .cloned()
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            let edges = phab_db::edges::load_edges(
                database,
                &missing,
                &[
                    phab_db::edges::REVISION_DEPENDS_ON_REVISION,
                    phab_db::edges::REVISION_DEPENDED_ON_BY_REVISION,
                ],
                None,
            )
            .await?;
            for node in missing {
                let of_type = |edge_type: i64| {
                    let mut destinations: Vec<Phid> = Vec::new();
                    for edge in edges
                        .iter()
                        .filter(|edge| edge.src == node && edge.edge_type == edge_type)
                    {
                        if !destinations.contains(&edge.dst) {
                            destinations.push(edge.dst.clone());
                        }
                    }
                    destinations
                };
                let parents = of_type(phab_db::edges::REVISION_DEPENDS_ON_REVISION);
                let children = of_type(phab_db::edges::REVISION_DEPENDED_ON_BY_REVISION);
                known.insert(node, (parents, children));
            }
        }
        for walk in &mut walks {
            let level = std::mem::take(&mut walk.level);
            for node in &level {
                let (parents, children) = &known[node];
                for next in parents.iter().chain(children) {
                    if walk.reached.insert(next.clone()) {
                        walk.level.push(next.clone());
                    }
                }
            }
            walk.order.extend(level);
        }
    }
    Ok(seeds
        .iter()
        .zip(walks)
        .map(|(seed, walk)| {
            let graph = walk
                .order
                .into_iter()
                .map(|node| {
                    let parents: Vec<Value> = known[&node]
                        .0
                        .iter()
                        .map(|parent| json!(parent.as_str()))
                        .collect();
                    (node.to_string(), Value::Array(parents))
                })
                .collect();
            (seed.clone(), graph)
        })
        .collect())
}

/// `DifferentialRevisionStatus`: the name, whether it is closed, and the
/// ANSI color, of a status key.
fn status_info(status: &str, close_on_accept: bool) -> Option<Value> {
    let (name, closed, color) = match status {
        "needs-review" => ("Needs Review", false, Some("magenta")),
        "needs-revision" => ("Needs Revision", false, Some("red")),
        "changes-planned" => ("Changes Planned", false, Some("red")),
        "accepted" => ("Accepted", close_on_accept, Some("green")),
        "published" => ("Closed", true, Some("cyan")),
        "abandoned" => ("Abandoned", true, None),
        "draft" => ("Draft", false, None),
        _ => return None,
    };
    Some(json!({"value": status, "name": name, "closed": closed, "color.ansi": color}))
}

/// `json_encode(phutil_json_decode($value))`: PHP has no empty objects,
/// and objects keyed "0", "1"... become lists.
fn php_array(value: &Value) -> Value {
    match value {
        Value::Array(items) => items.iter().map(php_array).collect(),
        Value::Object(entries) => {
            let is_list = entries
                .keys()
                .enumerate()
                .all(|(index, key)| *key == index.to_string());
            if is_list {
                entries.values().map(php_array).collect()
            } else {
                Value::Object(
                    entries
                        .iter()
                        .map(|(key, item)| (key.clone(), php_array(item)))
                        .collect(),
                )
            }
        }
        other => other.clone(),
    }
}

/// `DifferentialUpliftRequestCustomField::setValueFromStorage()`: the JSON
/// object or list, anything else (no value, invalid JSON, a scalar) being
/// an empty list.
fn uplift_request(stored: Option<&String>) -> Value {
    match stored.and_then(|stored| serde_json::from_str::<Value>(stored).ok()) {
        Some(value @ (Value::Object(_) | Value::Array(_))) => php_array(&value),
        _ => json!([]),
    }
}

/// A property of `properties`, PHP's `getProperty($key, $default)`.
fn property(properties: &Value, key: &str) -> Option<Value> {
    properties.get(key).cloned()
}

/// `differential.revision.search`.
pub struct RevisionSearch;

impl Method for RevisionSearch {
    fn name(&self) -> &'static str {
        "differential.revision.search"
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
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            check_unported(request)?;
            let constraints = Constraints::parse(request.value("constraints"))?;
            let paging = Paging::parse(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
                DEFAULT_LIMIT,
            )?;
            let (start, direction) = paging.start();
            let query = Revisions {
                request,
                constraints,
                memory: std::sync::Mutex::default(),
            };
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);

            let context = request.context;
            let database = &context.database;
            let extra = load_extra(database, &page.items).await?;
            let diffs = load_diff_ids(database, &page.items).await?;
            let custom = load_custom_fields(database, &page.items).await?;
            let seeds: Vec<Phid> = page
                .items
                .iter()
                .map(|revision| revision.phid.clone())
                .collect();
            let mut graphs = load_stack_graphs(database, &seeds).await?;
            let base_uri = context
                .config
                .nonempty_string("phabricator.base-uri")
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned();
            let close_on_accept = context.config.truthy("differential.close-on-accept");

            let mut data = Vec::new();
            for revision in &page.items {
                let extra = extra
                    .get(&revision.id)
                    .ok_or_else(|| MethodError::internal("revision row vanished"))?;
                let status = status_info(&revision.status, close_on_accept).ok_or(
                    MethodError::Unsupported("a revision with an unknown status"),
                )?;
                // loadActiveDiff()->getID() fails without a diff.
                let diff_id = revision
                    .active_diff_phid
                    .as_ref()
                    .and_then(|phid| diffs.get(phid))
                    .ok_or(MethodError::Unsupported("a revision without its diff"))?;
                let should_broadcast = property(&extra.properties, "draft.broadcast")
                    .is_none_or(|value| phab_core::php::is_truthy(&value));
                let hold_as_draft = property(&extra.properties, "draft.hold")
                    .is_some_and(|value| phab_core::php::is_truthy(&value));
                let bug = custom.get(&(revision.phid.clone(), BUG_FIELD));
                let uplift = custom.get(&(revision.phid.clone(), UPLIFT_FIELD));
                let fields = json!({
                    "stackGraph": graphs.remove(&revision.phid).unwrap_or_default(),
                    "title": revision.title,
                    "uri": format!("{base_uri}/D{}", revision.id),
                    "authorPHID": revision.author_phid.as_ref().map(Phid::as_str),
                    "status": status,
                    "repositoryPHID": revision.repository_phid.as_ref().map(Phid::as_str),
                    "diffPHID": revision.active_diff_phid.as_ref().map(Phid::as_str),
                    "diffID": diff_id,
                    "summary": extra.summary,
                    "testPlan": extra.test_plan,
                    "isDraft": !should_broadcast,
                    "holdAsDraft": hold_as_draft,
                    "dateCreated": extra.date_created,
                    "dateModified": revision.date_modified,
                    "policy": {"view": revision.view_policy, "edit": revision.edit_policy},
                    "bugzilla.bug-id": bug,
                    "uplift.request": uplift_request(uplift),
                });
                data.push(json!({
                    "id": revision.id,
                    "type": "DREV",
                    "phid": revision.phid.as_str(),
                    "fields": fields,
                    "attachments": {},
                }));
            }
            let cursor =
                |revision: Option<Revision>| revision.map(|revision| revision.id.to_string());
            Ok(json!({
                "data": data,
                "maps": {},
                "query": {"queryKey": null},
                "cursor": {
                    "limit": paging.size.shown,
                    "after": cursor(page.next),
                    "before": cursor(page.previous),
                    "order": request.value("order").cloned().unwrap_or(Value::Null),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_follow_php() {
        assert_eq!(Constraints::parse(None), Ok(Constraints::default()));
        assert_eq!(
            Constraints::parse(Some(&json!([]))),
            Ok(Constraints::default())
        );
        assert_eq!(
            Constraints::parse(Some(&json!({"ids": [3, 1], "phids": ["x"]}))),
            Ok(Constraints {
                ids: Some(vec![3, 1]),
                phids: Some(vec!["x".to_owned()])
            })
        );
        let error = |message: &str| Err(pager::core_error(message.to_owned()));
        for (constraints, message) in [
            (
                json!({"ids": [1], "nope": 1}),
                "Constraint \"nope\" is not a valid constraint for this query.",
            ),
            (
                json!(["x"]),
                "Constraint \"0\" is not a valid constraint for this query.",
            ),
            (
                json!({"ids": []}),
                "Error while reading \"ids\": Expected a nonempty list, but value is an empty \
                 list.",
            ),
            (
                json!({"phids": []}),
                "Error while reading \"phids\": Expected a nonempty list, but value is an \
                 empty list.",
            ),
            (
                json!({"ids": [1, "2"]}),
                "Error while reading \"ids[1]\": Expected integer, got something else.",
            ),
            (
                json!({"ids": [2.0]}),
                "Error while reading \"ids[0]\": Expected integer, got something else.",
            ),
            (
                json!({"ids": null}),
                "Error while reading \"ids\": Expected a list, but value is not a list.",
            ),
            (
                json!({"ids": {"a": 2}}),
                "Error while reading \"ids\": Expected a list, but value is an object.",
            ),
        ] {
            assert_eq!(
                Constraints::parse(Some(&constraints)),
                error(message),
                "{constraints}"
            );
        }
        for unported in [
            json!({"statuses": ["accepted"]}),
            json!({"ids": {"0": 1}}),
            json!({"phids": [1]}),
            json!("x"),
        ] {
            assert!(
                matches!(
                    Constraints::parse(Some(&unported)),
                    Err(MethodError::Unsupported(_))
                ),
                "{unported}"
            );
        }
    }

    #[test]
    fn statuses_and_custom_fields_like_php() {
        assert_eq!(
            status_info("accepted", true),
            Some(
                json!({"value": "accepted", "name": "Accepted", "closed": true, "color.ansi": "green"})
            )
        );
        assert_eq!(
            status_info("abandoned", false),
            Some(
                json!({"value": "abandoned", "name": "Abandoned", "closed": true, "color.ansi": null})
            )
        );
        assert_eq!(status_info("weird", false), None);
        assert_eq!(uplift_request(None), json!([]));
        assert_eq!(uplift_request(Some(&"{}".to_owned())), json!([]));
        assert_eq!(uplift_request(Some(&"null".to_owned())), json!([]));
        assert_eq!(uplift_request(Some(&"not json".to_owned())), json!([]));
        assert_eq!(
            uplift_request(Some(&r#"{"Risk": "low", "0": "x"}"#.to_owned())),
            json!({"Risk": "low", "0": "x"})
        );
    }
}
