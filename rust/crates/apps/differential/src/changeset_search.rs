//! `differential.changeset.search`: `DifferentialChangesetSearchEngine`
//! through `buildConduitResponse()`.
//!
//! Ported: the `diffPHIDs`, `ids` and `phids` constraints, the `newest`
//! and `oldest` orders, paging and the fields. Left to PHP: changesets of
//! diffs without a revision (PHP fails on them) and of diffs with a
//! source control path, saved queries and order vectors.

use std::collections::HashMap;
use std::fmt::Write;

use base64::Engine;
use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints, IdOrder};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::{POLICY_PUBLIC, POLICY_USER};
use serde_json::{Map, Value, json};

use crate::diff_search::{self, DiffRow};

const QUERY_CLASS: &str = "DifferentialChangesetQuery";

const VALID_CONSTRAINTS: &[&str] = &["diffPHIDs", "ids", "phids"];

const ORDERS: &[&str] = &["newest", "created", "oldest"];

/// `DifferentialChangeType::FILE_DIRECTORY`.
const FILE_DIRECTORY: i64 = 4;

/// A `differential_changeset` row, with its diff once filtered.
#[derive(Debug, Clone)]
struct ChangesetRow {
    id: i64,
    phid: Phid,
    diff_id: i64,
    filename: String,
    file_type: i64,
    date_created: i64,
    date_modified: i64,
    diff: Option<DiffRow>,
}

const COLUMNS: &str = "id, phid, diffID, filename, fileType, dateCreated, dateModified";

impl ChangesetRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            diff_id: rows::int(row, "diffID")?,
            filename: rows::text(row, "filename")?,
            file_type: rows::int(row, "fileType")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
            diff: None,
        })
    }

    /// `getDisplayFilename()`.
    fn display_path(&self) -> String {
        if self.file_type == FILE_DIRECTORY {
            format!("{}/", self.filename)
        } else {
            self.filename.clone()
        }
    }

    /// `getAbsoluteRepositoryPath()` without a source control path.
    fn absolute_path(&self) -> String {
        format!("/{}", self.filename.trim_start_matches('/'))
    }
}

/// `DifferentialChangesetQuery` with the ported constraints; `None` when
/// nothing can match.
struct Changesets<'a> {
    request: &'a ApiRequest<'a>,
    clauses: Option<Clauses>,
    order: IdOrder,
}

impl PolicyAwareQuery for Changesets<'_> {
    type Row = ChangesetRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &ChangesetRow) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<ChangesetRow>, MethodError> {
        let database = &self.request.context.database;
        // A cursor's own query has none of the constraints.
        let mut clauses = match (ids, &self.clauses) {
            (Some(ids), _) => {
                let mut clauses = Clauses::default();
                clauses.in_list("id", ids.iter().map(|id| json!(id)));
                clauses
            }
            (None, Some(clauses)) => clauses.clone(),
            (None, None) => return Ok(Vec::new()),
        };
        let (comparison, order) = search::id_paging(self.order, direction);
        if let Some(after) = after {
            clauses.condition(format!("id {comparison} ?"), [json!(after)]);
        }
        let mut tail = format!(" ORDER BY id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(
            &format!(
                "SELECT {COLUMNS} FROM {}",
                database.table("differential", "differential_changeset")
            ),
            &tail,
        );
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(ChangesetRow::from_row)
            .collect::<Result<_, _>>()?)
    }

    /// `willFilterPage()`: changesets of diffs the viewer can't see are
    /// dropped.
    async fn filter(&self, rows: Vec<ChangesetRow>) -> Result<Vec<ChangesetRow>, MethodError> {
        let mut diff_ids: Vec<i64> = rows.iter().map(|row| row.diff_id).collect();
        diff_ids.sort_unstable();
        diff_ids.dedup();
        let diffs: HashMap<i64, DiffRow> = diff_search::visible_diff_rows(self.request, &diff_ids)
            .await?
            .into_iter()
            .map(|diff| (diff.id, diff))
            .collect();
        Ok(rows
            .into_iter()
            .filter_map(|mut row| {
                row.diff = Some(diffs.get(&row.diff_id)?.clone());
                Some(row)
            })
            .collect())
    }
}

/// The diffs with these PHIDs, by ID (`withDiffPHIDs()` reads them
/// without policy).
async fn diff_ids(database: &Database, phids: &[String]) -> Result<Vec<i64>, MethodError> {
    let sql = format!(
        "SELECT id FROM {} WHERE phid IN ({})",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(phids.len())
    );
    Ok(phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| rows::int(row, "id"))
        .collect::<Result<_, _>>()?)
}

/// Fails over to PHP for what the page can't show like PHP: diffs without
/// a revision (`getRevision()` is not attached and PHP fails) and diffs
/// with a source control path.
async fn check_page(database: &Database, changesets: &[ChangesetRow]) -> Result<(), MethodError> {
    let diffs: Vec<&DiffRow> = changesets
        .iter()
        .filter_map(|row| row.diff.as_ref())
        .collect();
    if diffs.iter().any(|diff| diff.revision_id.is_none()) {
        return Err(MethodError::Unsupported(
            "changesets of diffs without a revision",
        ));
    }
    if diffs.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "SELECT 1 FROM {} WHERE id IN ({}) AND sourceControlPath IS NOT NULL \
         AND sourceControlPath != '' LIMIT 1",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(diffs.len())
    );
    let with_paths =
        phab_db::bind_all(phab_db::sqlx::query(&sql), diffs.iter().map(|diff| diff.id))
            .fetch_all(database.pool())
            .await?;
    if !with_paths.is_empty() {
        return Err(MethodError::Unsupported("diffs with a source control path"));
    }
    Ok(())
}

/// One result: `getFieldValuesForConduit()` and the extensions' fields.
fn result(changeset: &ChangesetRow, most_open: &str) -> Result<Value, MethodError> {
    let diff = changeset
        .diff
        .as_ref()
        .ok_or_else(|| MethodError::internal("changeset without its diff"))?;
    let absolute = base64::engine::general_purpose::STANDARD.encode(changeset.absolute_path());
    Ok(json!({
        "id": changeset.id,
        "type": "DCNG",
        "phid": changeset.phid.as_str(),
        "fields": {
            "diffPHID": diff.phid.as_str(),
            "path": {
                "displayPath": changeset.display_path(),
                "absolutePath.base64": absolute,
            },
            "dateCreated": changeset.date_created,
            "dateModified": changeset.date_modified,
            // Diffs of a revision have the most open policy.
            "policy": {"view": most_open},
        },
        "attachments": Map::new(),
    }))
}

/// `differential.changeset.search`.
pub struct ChangesetSearch;

impl Method for ChangesetSearch {
    fn name(&self) -> &'static str {
        "differential.changeset.search"
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

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            search::check_query_key(request)?;
            let constraints = Constraints::parse(request.value("constraints"), VALID_CONSTRAINTS)?;
            let strict = request.strictly_typed;
            let diff_phids = constraints
                .get(&["diffPHIDs"])
                .map(|(key, value)| search::string_list(key, value, false))
                .transpose()?;
            let ids = constraints
                .get(&["ids"])
                .map(|(key, value)| search::int_list(key, value, strict))
                .transpose()?;
            let phids = constraints
                .get(&["phids"])
                .map(|(key, value)| search::string_list(key, value, false))
                .transpose()?;
            search::requested_attachments(request.value("attachments"), &[])?;
            let order = IdOrder::parse(request.value("order"), QUERY_CLASS, ORDERS)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let database = &request.context.database;
            let mut clauses = Clauses::default();
            let mut empty = false;
            if let Some(ids) = &ids {
                clauses.in_list("id", ids.iter().map(|id| json!(id)));
            }
            if let Some(phids) = &phids {
                clauses.in_list("phid", search::strings(phids));
            }
            if let Some(diff_phids) = &diff_phids {
                let diffs = diff_ids(database, diff_phids).await?;
                empty = diffs.is_empty();
                clauses.in_list("diffID", diffs.iter().map(|id| json!(id)));
            }
            let query = Changesets {
                request,
                clauses: (!empty).then_some(clauses),
                order,
            };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            check_page(database, &page.items).await?;
            let most_open = if request.context.config.allow_public() {
                POLICY_PUBLIC
            } else {
                POLICY_USER
            };
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|changeset| result(changeset, most_open))
                .collect::<Result<_, _>>()?;
            Ok(search::response(
                request,
                &data,
                &paging,
                &page,
                |changeset| changeset.id.to_string(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let changeset = |filename: &str, file_type| ChangesetRow {
            id: 1,
            phid: Phid::new("PHID-DCNG-1"),
            diff_id: 1,
            filename: filename.to_owned(),
            file_type,
            date_created: 1,
            date_modified: 1,
            diff: None,
        };
        assert_eq!(changeset("a/b.rs", 1).display_path(), "a/b.rs");
        assert_eq!(changeset("dom", FILE_DIRECTORY).display_path(), "dom/");
        assert_eq!(changeset("a/b.rs", 1).absolute_path(), "/a/b.rs");
        assert_eq!(changeset("/a/b.rs", 1).absolute_path(), "/a/b.rs");
    }
}
