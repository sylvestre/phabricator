//! `flag.query` (`FlagQueryConduitAPIMethod`): the viewer's flags
//! (`PhabricatorFlagQuery`), with the handles of the flagged objects.
//!
//! Ported: every parameter. Left to PHP: flags of objects whose handles
//! are not ported (`phab_app_phid::handles`) or which the viewer can't
//! see (PHP shows a placeholder handle), and values of unusual types.

use std::fmt::Write;

use phab_app_phid::handles::{self, Handle};
use phab_conduit::pager::{self, Direction, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, IdOrder};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{DbError, rows};
use serde_json::{Value, json};

/// `PhabricatorFlagColor::getColorNameMap()`.
pub(crate) fn color_name(color: i64) -> &'static str {
    match color {
        0 => "Red",
        1 => "Orange",
        2 => "Yellow",
        3 => "Green",
        4 => "Blue",
        5 => "Pink",
        6 => "Purple",
        7 => "Checkered",
        _ => "Unknown",
    }
}

/// A `flag` row (`PhabricatorFlag`).
#[derive(Debug, Clone)]
pub struct Flag {
    pub(crate) id: i64,
    pub(crate) owner_phid: String,
    pub(crate) kind: String,
    pub(crate) object_phid: String,
    pub(crate) reason_phid: String,
    pub(crate) color: i64,
    pub(crate) note: String,
    pub(crate) date_created: i64,
    pub(crate) date_modified: i64,
}

impl Flag {
    pub(crate) fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            owner_phid: rows::text(row, "ownerPHID")?,
            kind: rows::text(row, "type")?,
            object_phid: rows::text(row, "objectPHID")?,
            reason_phid: rows::text(row, "reasonPHID")?,
            color: rows::int(row, "color")?,
            note: rows::text(row, "note")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
        })
    }

    /// `FlagConduitAPIMethod::buildFlagInfoDictionary()`; Lisk keeps
    /// MySQL's strings.
    pub(crate) fn dictionary(&self, handle: &Handle, production_uri: &str) -> Value {
        json!({
            "id": self.id.to_string(),
            "ownerPHID": self.owner_phid,
            "type": self.kind,
            "objectPHID": self.object_phid,
            "reasonPHID": self.reason_phid,
            "color": self.color.to_string(),
            "colorName": color_name(self.color),
            "note": self.note,
            "handle": {
                "uri": format!("{}{}", production_uri.trim_end_matches('/'), handle.uri),
                "name": handle.name,
                "fullname": handle.full_name,
            },
            "dateCreated": self.date_created.to_string(),
            "dateModified": self.date_modified.to_string(),
        })
    }
}

/// The constraints of `PhabricatorFlagQuery`.
#[derive(Debug, Default)]
struct Filters {
    owners: Option<Vec<String>>,
    types: Option<Vec<String>>,
    objects: Option<Vec<String>>,
}

/// `PhabricatorFlagQuery` with these constraints, in its default order
/// (by decreasing ID).
struct Flags<'a> {
    request: &'a ApiRequest<'a>,
    filters: Filters,
}

impl PolicyAwareQuery for Flags<'_> {
    type Row = Flag;

    fn query_class(&self) -> &'static str {
        "PhabricatorFlagQuery"
    }

    fn id(row: &Flag) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<Flag>, MethodError> {
        let database = &self.request.context.database;
        let mut clauses = Clauses::default();
        if let Some(ids) = ids {
            clauses.in_list("flag.id", ids.iter().map(|id| json!(id)));
        } else {
            for (column, values) in [
                ("flag.ownerPHID", &self.filters.owners),
                ("flag.type", &self.filters.types),
                ("flag.objectPHID", &self.filters.objects),
            ] {
                if let Some(values) = values {
                    clauses.in_list(column, search::strings(values));
                }
            }
        }
        let (comparison, order) = search::id_paging(IdOrder::Newest, direction);
        if let Some(after) = after {
            clauses.condition(format!("flag.id {comparison} ?"), [json!(after)]);
        }
        let select = format!("SELECT * FROM {} flag", database.table("flag", "flag"));
        let mut tail = format!(" ORDER BY flag.id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(&select, &tail);
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(Flag::from_row)
            .collect::<Result<_, _>>()?)
    }

    /// Flags are private: only their owner sees them.
    fn filter(
        &self,
        rows: Vec<Flag>,
    ) -> impl Future<Output = Result<Vec<Flag>, MethodError>> + Send {
        let viewer = &self.request.viewer;
        std::future::ready(Ok(rows
            .into_iter()
            .filter(|flag| {
                viewer.is_omnipotent
                    || viewer
                        .phid
                        .as_ref()
                        .is_some_and(|phid| phid.as_str() == flag.owner_phid)
            })
            .collect()))
    }
}

/// An `optional list<...>` parameter: falsy is no constraint.
fn string_list(request: &ApiRequest<'_>, key: &str) -> Result<Option<Vec<String>>, MethodError> {
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

/// `PhabricatorEnv::getProductionURI()` of a path.
pub(crate) fn production_uri(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    let config = &request.context.config;
    match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => Ok(uri.to_owned()),
        None => config
            .base_uri()
            .map(str::to_owned)
            .map_err(MethodError::internal),
    }
}

/// `flag.query`.
pub struct FlagQuery;

impl Method for FlagQuery {
    fn name(&self) -> &'static str {
        "flag.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &["ownerPHIDs", "types", "objectPHIDs", "offset", "limit"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let filters = Filters {
                owners: string_list(request, "ownerPHIDs")?,
                objects: string_list(request, "objectPHIDs")?,
                types: string_list(request, "types")?,
            };
            let offset = pager::offset_value(request.value("offset"), 0)?;
            let limit = pager::offset_value(request.value("limit"), 100)?;
            let query = Flags { request, filters };
            let flags = pager::execute_with_offset(&query, offset, limit).await?;

            // needHandles(): every flag gets the handle of its object.
            let mut phids: Vec<Phid> = flags
                .iter()
                .map(|flag| Phid::new(flag.object_phid.clone()))
                .collect();
            phids.sort();
            phids.dedup();
            let found = handles::load(request, &phids).await?;
            let base = production_uri(request)?;
            flags
                .iter()
                .map(|flag| {
                    let handle = found.get(&Phid::new(flag.object_phid.clone())).ok_or(
                        MethodError::Unsupported("flags of objects the viewer can't see"),
                    )?;
                    Ok(flag.dictionary(handle, &base))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_colors() {
        assert_eq!(color_name(0), "Red");
        assert_eq!(color_name(7), "Checkered");
        assert_eq!(color_name(8), "Unknown");
    }
}
