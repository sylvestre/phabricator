//! `edge.search`: the edges of revisions and commits
//! (`EdgeSearchConduitAPIMethod` and `PhabricatorEdgeObjectQuery`).
//!
//! Edges have the most open policy; their sources must be visible. Sources
//! other than revisions and commits, and object names other than PHIDs,
//! are left to PHP.

use std::collections::HashMap;
use std::fmt::Write;

use phab_conduit::order::{self, Column};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture, pager};
use phab_core::{Phid, PhidType};
use phab_db::{edges, rows};
use serde_json::{Value, json};

use crate::{commit, storage};

/// The edge types with a Conduit key (`getConduitKey()`), by key.
const EDGE_TYPES: &[(&str, i64)] = &[
    ("commit.revision", edges::COMMIT_HAS_REVISION),
    ("mention", edges::OBJECT_MENTIONS_OBJECT),
    ("mentioned-in", edges::OBJECT_MENTIONED_BY_OBJECT),
    ("revision.child", edges::REVISION_DEPENDED_ON_BY_REVISION),
    ("revision.commit", edges::REVISION_HAS_COMMIT),
    ("revision.parent", edges::REVISION_DEPENDS_ON_REVISION),
];

const DATE: Column = Column::new("dateCreated");
const SEQUENCE: Column = Column::new("seq");

/// `ConduitAPIMethod::newPager()`: the limit, 100 by default, between 1
/// and 1000.
pub(crate) fn limit(value: Option<&Value>) -> Result<usize, MethodError> {
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

/// An edge cursor, `<dateCreated>_<seq>`.
fn parse_cursor(value: Option<&Value>) -> Result<Option<(i64, i64)>, MethodError> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text,
        Some(_) => return Err(MethodError::Unsupported("an unusual cursor")),
    };
    let parsed = text.split_once('_').and_then(|(epoch, sequence)| {
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        (digits(epoch) && digits(sequence))
            .then(|| Some((epoch.parse().ok()?, sequence.parse().ok()?)))
            .flatten()
    });
    // PhabricatorInvalidQueryCursorException is left to PHP.
    parsed
        .map(Some)
        .ok_or(MethodError::Unsupported("an invalid edge cursor"))
}

/// A list parameter of PHIDs or edge types.
fn strings(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// The sources the viewer can see (`PhabricatorObjectQuery::withNames()`),
/// in the order of the request; any other is an error.
async fn visible_sources(
    request: &ApiRequest<'_>,
    names: &[String],
) -> Result<Vec<Phid>, MethodError> {
    let context = request.context;
    let database = &context.database;
    let phids: Vec<Phid> = names.iter().cloned().map(Phid::new).collect();
    let mut kinds = phids.iter().map(Phid::phid_type);
    let visible: Vec<Phid> = match kinds.next() {
        None => Vec::new(),
        Some(PhidType::REVISION) if kinds.all(|kind| kind == PhidType::REVISION) => {
            let revisions = storage::load_revisions(database, &phids).await?;
            storage::visible_revisions(
                database,
                &context.config,
                &request.viewer,
                revisions,
                request.now,
            )
            .await
            .map_err(MethodError::internal)?
            .into_iter()
            .map(|revision| revision.phid)
            .collect()
        }
        Some(PhidType::COMMIT) if kinds.all(|kind| kind == PhidType::COMMIT) => {
            commit::load_visible_by_phids(
                database,
                &context.config,
                &request.viewer,
                &phids,
                request.now,
            )
            .await
            .map_err(MethodError::internal)?
            .into_keys()
            .collect()
        }
        Some(_) => return Err(MethodError::Unsupported("edges of other objects")),
    };
    for phid in &phids {
        if !visible.contains(phid) {
            return Err(pager::core_error(format!(
                "Source PHID \"{}\" does not identify a valid object, or you do not have \
                 permission to view it.",
                phid.as_str()
            )));
        }
    }
    let mut unique = Vec::new();
    for phid in phids {
        if !unique.contains(&phid) {
            unique.push(phid);
        }
    }
    Ok(unique)
}

/// An edge with what pages it.
#[derive(Debug, Clone)]
struct EdgeRow {
    source: String,
    edge_type: i64,
    destination: String,
    date_created: i64,
    sequence: i64,
}

impl EdgeRow {
    fn cursor(&self) -> String {
        format!("{}_{}", self.date_created, self.sequence)
    }
}

/// The edge type constants of the Conduit keys, with the key of each.
fn edge_constants(types: &[String]) -> Result<(Vec<i64>, HashMap<i64, &'static str>), MethodError> {
    let mut constants = Vec::new();
    let mut keys: HashMap<i64, &'static str> = HashMap::new();
    for name in types {
        let Some((key, constant)) = EDGE_TYPES.iter().find(|(key, _)| key == name) else {
            return Err(pager::core_error(format!(
                "Edge type \"{name}\" is not a recognized edge type."
            )));
        };
        constants.push(*constant);
        keys.insert(*constant, key);
    }
    Ok((constants, keys))
}

/// `PhabricatorEdgeObjectQuery` for one page.
struct EdgeQuery<'a> {
    sources: &'a [Phid],
    constants: &'a [i64],
    destinations: &'a [String],
    /// `dateCreated` and `seq` of the edge the page starts after.
    cursor: Option<(i64, i64)>,
    reversed: bool,
    limit: usize,
}

impl EdgeQuery<'_> {
    /// The edges of the page, in the order of the query.
    async fn load(&self, database: &phab_db::Database) -> Result<Vec<EdgeRow>, MethodError> {
        let table = edges::edge_database(self.sources[0].phid_type())
            .map(|name| database.table(name, "edge"))
            .ok_or(MethodError::Unsupported("edges of other objects"))?;
        let mut sql = format!(
            "SELECT src, type, dst, dateCreated, seq FROM {table} WHERE (src IN ({})) \
             AND (type IN ({}))",
            phab_db::placeholders(self.sources.len()),
            phab_db::placeholders(self.constants.len()),
        );
        let mut binds: Vec<Value> = self
            .sources
            .iter()
            .map(|phid| json!(phid.as_str()))
            .collect();
        binds.extend(
            self.constants
                .iter()
                .map(|constant| json!(constant.to_string())),
        );
        if !self.destinations.is_empty() {
            let _ = write!(
                sql,
                " AND (dst IN ({}))",
                phab_db::placeholders(self.destinations.len())
            );
            binds.extend(self.destinations.iter().map(|phid| json!(phid)));
        }
        let columns = [DATE, SEQUENCE];
        if let Some((epoch, sequence)) = self.cursor
            && let Some((clause, values)) =
                order::paging_clause(&columns, &[json!(epoch), json!(sequence)], self.reversed)
        {
            let _ = write!(sql, " AND ({clause})");
            binds.extend(values);
        }
        sql.push_str(&order::order_by(&columns, self.reversed));
        let _ = write!(sql, " LIMIT {}", self.limit);
        let mut query = phab_db::sqlx::query(&sql);
        for bind in &binds {
            query = match bind {
                Value::String(text) => query.bind(text.as_str()),
                other => query.bind(other.as_i64()),
            };
        }
        let mut found: Vec<EdgeRow> = query
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| {
                Ok(EdgeRow {
                    source: rows::text(row, "src")?,
                    edge_type: rows::int(row, "type")?,
                    destination: rows::text(row, "dst")?,
                    date_created: rows::int(row, "dateCreated")?,
                    sequence: rows::int(row, "seq")?,
                })
            })
            .collect::<Result<_, phab_db::DbError>>()?;
        if self.reversed {
            found.reverse();
        }
        Ok(found)
    }
}

/// `edge.search`.
pub struct EdgeSearch;

impl Method for EdgeSearch {
    fn name(&self) -> &'static str {
        "edge.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "sourcePHIDs",
            "types",
            "destinationPHIDs",
            "before",
            "after",
            "limit",
        ]
    }

    /// An infrastructure method, of no application.
    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let page_size = limit(request.value("limit"))?;
            let before = parse_cursor(request.value("before"))?;
            let after = parse_cursor(request.value("after"))?;
            let names = strings(request.value("sourcePHIDs"))?;
            let types = strings(request.value("types"))?;
            let destinations = strings(request.value("destinationPHIDs"))?;
            let sources = visible_sources(request, &names).await?;
            if types.is_empty() {
                return Err(pager::core_error(
                    "Edge search must specify a nonempty list of edge types.".to_owned(),
                ));
            }
            let (constants, keys) = edge_constants(&types)?;
            if sources.is_empty() {
                return Err(pager::core_error(
                    "Edge object query must be executed with a nonempty list of source PHIDs."
                        .to_owned(),
                ));
            }
            // "after" wins over "before"; paging backwards reverses the
            // order, then the results.
            let (cursor, reversed) = match (after, before) {
                (Some(after), _) => (Some(after), false),
                (None, Some(before)) => (Some(before), true),
                (None, None) => (None, false),
            };
            let query = EdgeQuery {
                sources: &sources,
                constants: &constants,
                destinations: &destinations,
                cursor,
                reversed,
                limit: page_size + 1,
            };
            let found = query.load(&request.context.database).await?;
            let pager = pager::Paging {
                size: pager::PageSize {
                    shown: json!(page_size),
                    value: page_size,
                },
                after: after.map(|_| pager::Cursor {
                    text: String::new(),
                    truthy: true,
                }),
                before: before.map(|_| pager::Cursor {
                    text: String::new(),
                    truthy: true,
                }),
            }
            .pager();
            let page = pager.slice(found);
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|edge| {
                    json!({
                        "sourcePHID": edge.source,
                        "edgeType": keys.get(&edge.edge_type).copied(),
                        "destinationPHID": edge.destination,
                    })
                })
                .collect();
            Ok(json!({
                "data": data,
                "cursor": {
                    "limit": page_size,
                    "after": page.next.as_ref().map(EdgeRow::cursor),
                    "before": page.previous.as_ref().map(EdgeRow::cursor),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_cursors() {
        assert_eq!(limit(None), Ok(100));
        assert_eq!(limit(Some(&json!(0))), Ok(1));
        assert_eq!(limit(Some(&json!(5000))), Ok(1000));
        assert_eq!(parse_cursor(Some(&json!("12_3"))), Ok(Some((12, 3))));
        assert!(parse_cursor(Some(&json!("12"))).is_err());
        assert_eq!(parse_cursor(None), Ok(None));
    }
}
