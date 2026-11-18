//! `differential.query` (`DifferentialQueryConduitAPIMethod`): the legacy
//! revision query, through the `differential.revision.search` query.
//!
//! Ported: `ids`, `phids`, `authors`, `reviewers`, `ccs`, `subscribers`,
//! `responsibleUsers`, `status`, `order`, `limit` and `offset`, with every
//! result field. Left to PHP: `commitHashes`, `branches`, `paths`, the
//! `none()` reviewer and values of unusual types.

use std::collections::HashMap;

use phab_conduit::pager::{self, Direction};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::{Database, edges, rows};
use serde_json::{Map, Value, json};

use crate::search::{self, Constraints, Order, Revisions};
use crate::storage::Revision;

/// A `list<phid>` or `list<string>` parameter; falsy is no constraint.
fn string_list(value: Option<&Value>) -> Result<Option<Vec<String>>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(list)) if list.is_empty() => Ok(None),
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// A `list<uint>` parameter; falsy is no constraint.
fn id_list(value: Option<&Value>) -> Result<Option<Vec<i64>>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(list)) if list.is_empty() => Ok(None),
        Some(Value::Array(list)) => list
            .iter()
            .map(|id| match id {
                Value::Number(number) if !number.is_f64() => number.as_i64(),
                Value::String(text)
                    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    text.parse().ok()
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported("IDs which are not integers")),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// An `optional uint` parameter; falsy is none.
fn count(value: Option<&Value>) -> Result<usize, MethodError> {
    let count = match value {
        None | Some(Value::Null) => return Ok(0),
        Some(Value::Number(number)) if !number.is_f64() => number.as_u64(),
        Some(Value::String(text)) if text.is_empty() => return Ok(0),
        Some(Value::String(text)) if text.bytes().all(|b| b.is_ascii_digit()) => text.parse().ok(),
        _ => None,
    };
    count
        .and_then(|count| usize::try_from(count).ok())
        .ok_or(MethodError::Unsupported("an unusual limit or offset"))
}

/// `DifferentialLegacyQuery::getModernValues()`: `None` for any status.
/// Accepted revisions are closed with `differential.close-on-accept`.
fn legacy_statuses(
    status: &str,
    close_on_accept: bool,
) -> Result<Option<Vec<String>>, MethodError> {
    let statuses: &[&str] = match (status, close_on_accept) {
        ("status-any", _) => return Ok(None),
        ("status-open", false) => &[
            "needs-review",
            "needs-revision",
            "changes-planned",
            "accepted",
            "draft",
        ],
        ("status-open", true) => &["needs-review", "needs-revision", "changes-planned", "draft"],
        ("status-accepted", _) => &["accepted"],
        ("status-needs-review", _) => &["needs-review", "draft"],
        ("status-needs-revision", _) => &["needs-revision"],
        ("status-closed", false) => &["published", "abandoned"],
        ("status-closed", true) => &["accepted", "published", "abandoned"],
        ("status-abandoned", _) => &["abandoned"],
        (other, _) => {
            return Err(pager::core_error(format!(
                "Unknown revision status filter constant \"{other}\"."
            )));
        }
    };
    Ok(Some(
        statuses.iter().map(|&status| status.to_owned()).collect(),
    ))
}

/// The constraints of the parameters, as `execute()` applies them.
fn constraints(request: &ApiRequest<'_>) -> Result<Constraints, MethodError> {
    for key in ["paths", "commitHashes", "branches"] {
        if request.value(key).is_some_and(php::is_truthy) {
            return Err(MethodError::Unsupported(
                "a differential.query parameter not ported",
            ));
        }
    }
    let reviewers = string_list(request.value("reviewers"))?;
    if reviewers
        .iter()
        .flatten()
        .any(|reviewer| reviewer == "none()")
    {
        return Err(MethodError::Unsupported("revisions without reviewers"));
    }
    let mut constraints = Constraints {
        ids: id_list(request.value("ids"))?,
        phids: string_list(request.value("phids"))?,
        authors: string_list(request.value("authors"))?,
        reviewers,
        responsibles: string_list(request.value("responsibleUsers"))?,
        // "subscribers" replaces "ccs": both are withCCs().
        subscribers: string_list(request.value("ccs"))?,
        ..Constraints::default()
    };
    if let Some(subscribers) = string_list(request.value("subscribers"))? {
        constraints.subscribers = Some(subscribers);
    }
    match request.value("status") {
        None | Some(Value::Null) => {}
        Some(Value::String(status)) if status.is_empty() => {}
        Some(Value::String(status)) => {
            constraints.statuses = legacy_statuses(
                status,
                request
                    .context
                    .config
                    .truthy("differential.close-on-accept"),
            )?
            .filter(|list| !list.is_empty());
        }
        Some(_) => return Err(MethodError::Unsupported("an unusual status")),
    }
    Ok(constraints)
}

/// What the results show beyond the revision rows.
struct ResultData {
    extra: HashMap<i64, search::Extra>,
    diffs: HashMap<i64, Vec<i64>>,
    active: HashMap<Phid, ActiveDiff>,
    line_counts: HashMap<i64, Option<i64>>,
    reviewers: HashMap<Phid, Vec<search::ReviewerRow>>,
    commits: HashMap<Phid, Vec<Phid>>,
    ccs: HashMap<Phid, Vec<Phid>>,
    /// The project and depends-on edges, by revision and type.
    auxiliary_edges: HashMap<(Phid, i64), Vec<Phid>>,
    hashes: HashMap<i64, Vec<(String, String)>>,
    custom: HashMap<(Phid, &'static str), String>,
}

/// The active diff's columns the results show.
struct ActiveDiff {
    branch: Option<String>,
    repository: Option<String>,
    source_path: Option<String>,
}

async fn load_active_diffs(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<Phid, ActiveDiff>, MethodError> {
    let phids: Vec<&str> = revisions
        .iter()
        .filter_map(|revision| revision.active_diff_phid.as_ref())
        .map(Phid::as_str)
        .collect();
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT phid, branch, repositoryPHID, sourcePath FROM {} WHERE phid IN ({})",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(phids.len())
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "phid")?),
                ActiveDiff {
                    branch: rows::opt_text(row, "branch")?,
                    repository: rows::opt_text(row, "repositoryPHID")?,
                    source_path: rows::opt_text(row, "sourcePath")?,
                },
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// The project and depends-on edges of the revisions, loaded in one query
/// like `loadCustomFieldsForRevisions()`: edges created at the same time
/// come in the order MySQL gives them for that query.
async fn load_auxiliary_edges(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<(Phid, i64), Vec<Phid>>, MethodError> {
    let sources: Vec<Phid> = revisions
        .iter()
        .map(|revision| revision.phid.clone())
        .collect();
    let mut lists: HashMap<(Phid, i64), Vec<Phid>> = HashMap::new();
    let types = [
        edges::OBJECT_HAS_PROJECT,
        edges::REVISION_DEPENDS_ON_REVISION,
    ];
    for edge in edges::load_edges(database, &sources, &types, None).await? {
        let list = lists.entry((edge.src, edge.edge_type)).or_default();
        if !list.contains(&edge.dst) {
            list.push(edge.dst);
        }
    }
    Ok(lists)
}

/// `loadDiffIDs()`: newest first.
async fn load_diff_ids(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<i64, Vec<i64>>, MethodError> {
    let sql = format!(
        "SELECT revisionID, id FROM {} WHERE revisionID IN ({}) ORDER BY id DESC",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(revisions.len())
    );
    let mut ids: HashMap<i64, Vec<i64>> = HashMap::new();
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.id),
    )
    .fetch_all(database.pool())
    .await?;
    for row in &found {
        ids.entry(rows::int(row, "revisionID")?)
            .or_default()
            .push(rows::int(row, "id")?);
    }
    Ok(ids)
}

/// `loadHashes()`: `(type, hash)` pairs.
async fn load_hashes(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<i64, Vec<(String, String)>>, MethodError> {
    let sql = format!(
        "SELECT revisionID, type, hash FROM {} WHERE revisionID IN ({})",
        database.table("differential", "differential_revisionhash"),
        phab_db::placeholders(revisions.len())
    );
    let mut hashes: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.id),
    )
    .fetch_all(database.pool())
    .await?;
    for row in &found {
        hashes
            .entry(rows::int(row, "revisionID")?)
            .or_default()
            .push((rows::text(row, "type")?, rows::text(row, "hash")?));
    }
    Ok(hashes)
}

async fn load_line_counts(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<i64, Option<i64>>, MethodError> {
    let sql = format!(
        "SELECT id, lineCount FROM {} WHERE id IN ({})",
        database.table("differential", "differential_revision"),
        phab_db::placeholders(revisions.len())
    );
    phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.id),
    )
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(|row| Ok((rows::int(row, "id")?, rows::opt_int(row, "lineCount")?)))
    .collect::<Result<_, phab_db::DbError>>()
    .map_err(Into::into)
}

impl ResultData {
    async fn load(database: &Database, revisions: &[Revision]) -> Result<Self, MethodError> {
        Ok(Self {
            extra: search::load_extra(database, revisions).await?,
            diffs: load_diff_ids(database, revisions).await?,
            active: load_active_diffs(database, revisions).await?,
            line_counts: load_line_counts(database, revisions).await?,
            reviewers: search::load_reviewers(database, revisions).await?,
            commits: search::load_edge_lists(database, revisions, edges::REVISION_HAS_COMMIT)
                .await?,
            ccs: search::load_edge_lists(database, revisions, edges::OBJECT_HAS_SUBSCRIBER).await?,
            auxiliary_edges: load_auxiliary_edges(database, revisions).await?,
            hashes: load_hashes(database, revisions).await?,
            custom: search::load_custom_fields(database, revisions).await?,
        })
    }

    fn auxiliary(&self, revision: &Revision, edge_type: i64) -> Vec<&str> {
        self.auxiliary_edges
            .get(&(revision.phid.clone(), edge_type))
            .into_iter()
            .flatten()
            .map(Phid::as_str)
            .collect()
    }

    /// One result; `None` for a revision without an active diff.
    fn result(
        &self,
        request: &ApiRequest<'_>,
        revision: &Revision,
        production_uri: &str,
    ) -> Result<Option<Value>, MethodError> {
        let Some(diff) = revision
            .active_diff_phid
            .as_ref()
            .and_then(|phid| Some((phid, self.active.get(phid)?)))
        else {
            return Ok(None);
        };
        let (diff_phid, diff) = diff;
        let extra = self
            .extra
            .get(&revision.id)
            .ok_or_else(|| MethodError::internal("revision row vanished"))?;
        let (legacy, name) = legacy_status(&revision.status).ok_or(MethodError::Unsupported(
            "a revision with an unknown status",
        ))?;
        let phids = |map: &HashMap<Phid, Vec<Phid>>| -> Vec<String> {
            map.get(&revision.phid)
                .into_iter()
                .flatten()
                .map(|phid| phid.as_str().to_owned())
                .collect()
        };
        let reviewers: Map<String, Value> = self
            .reviewers
            .get(&revision.phid)
            .into_iter()
            .flatten()
            .map(|row| (row.reviewer.clone(), json!(row.reviewer)))
            .collect();
        let reviewers = if reviewers.is_empty() {
            json!([])
        } else {
            Value::Object(reviewers)
        };
        let hashes: Vec<Value> = self
            .hashes
            .get(&revision.id)
            .into_iter()
            .flatten()
            .map(|(kind, hash)| json!([kind, hash]))
            .collect();
        let custom = |key| self.custom.get(&(revision.phid.clone(), key));
        let properties = php::round_trip(&extra.properties);
        // Lisk keeps MySQL's strings: integers are strings here.
        let mut result = json!({
            "id": revision.id.to_string(),
            "phid": revision.phid.as_str(),
            "title": revision.title,
            "uri": format!("{}/D{}", production_uri.trim_end_matches('/'), revision.id),
            "dateCreated": extra.date_created.to_string(),
            "dateModified": revision.date_modified.to_string(),
            "authorPHID": revision.author_phid.as_ref().map(Phid::as_str),
            "status": legacy.to_string(),
            "statusName": name,
            "properties": properties,
            "branch": diff.branch,
            "summary": extra.summary,
            "testPlan": extra.test_plan,
            "lineCount": self
                .line_counts
                .get(&revision.id)
                .copied()
                .flatten()
                .map(|count| count.to_string()),
            "activeDiffPHID": diff_phid.as_str(),
            "diffs": self
                .diffs
                .get(&revision.id)
                .into_iter()
                .flatten()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "commits": phids(&self.commits),
            "reviewers": reviewers,
            "ccs": phids(&self.ccs),
            "hashes": hashes,
            "auxiliary": {
                "bugzilla.bug-id": custom(search::BUG_FIELD),
                "uplift.request": search::uplift_request(custom(search::UPLIFT_FIELD)),
                "phabricator:projects": self.auxiliary(revision, edges::OBJECT_HAS_PROJECT),
                "phabricator:depends-on": self.auxiliary(
                    revision,
                    edges::REVISION_DEPENDS_ON_REVISION,
                ),
            },
            "repositoryPHID": diff.repository,
        });
        // Only the author sees the source path.
        if revision.author_phid.is_some() && revision.author_phid == request.viewer.phid {
            result["sourcePath"] = json!(diff.source_path);
        }
        Ok(Some(result))
    }
}

/// `getLegacyRevisionStatus()` and `getStatusDisplayName()`.
fn legacy_status(status: &str) -> Option<(i64, &'static str)> {
    Some(match status {
        "needs-review" => (0, "Needs Review"),
        "needs-revision" => (1, "Needs Revision"),
        "changes-planned" => (5, "Changes Planned"),
        "accepted" => (2, "Accepted"),
        "published" => (3, "Closed"),
        "abandoned" => (4, "Abandoned"),
        "draft" => (0, "Draft"),
        _ => return None,
    })
}

/// `differential.query`.
pub struct LegacyQuery;

impl Method for LegacyQuery {
    fn name(&self) -> &'static str {
        "differential.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "authors",
            "ccs",
            "reviewers",
            "paths",
            "commitHashes",
            "status",
            "order",
            "limit",
            "offset",
            "ids",
            "phids",
            "subscribers",
            "responsibleUsers",
            "branches",
        ]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let constraints = constraints(request)?;
            let order = match request.value("order") {
                Some(value) if php::is_truthy(value) => Order::parse(Some(value))?,
                // getDefaultOrderVector(): by update, unlike the search
                // engine's default.
                _ => Order::Updated,
            };
            let limit = count(request.value("limit"))?;
            let offset = count(request.value("offset"))?;
            let query = Revisions {
                request,
                constraints,
                order,
                memory: std::sync::Mutex::default(),
            };
            // PhabricatorPolicyAwareQuery::execute(): the first `offset`
            // visible results are skipped; no limit is no limit.
            let need = if limit == 0 {
                usize::MAX
            } else {
                offset.saturating_add(limit)
            };
            let mut revisions = pager::execute(&query, None, Direction::Forward, need).await?;
            revisions.drain(..offset.min(revisions.len()));
            let database = &request.context.database;
            if revisions.is_empty() {
                return Ok(json!([]));
            }
            let data = ResultData::load(database, &revisions).await?;
            let config = &request.context.config;
            let production_uri = match config.nonempty_string("phabricator.production-uri") {
                Some(uri) => uri.to_owned(),
                None => config
                    .base_uri()
                    .map(str::to_owned)
                    .map_err(MethodError::internal)?,
            };
            let mut results = Vec::new();
            for revision in &revisions {
                if let Some(result) = data.result(request, revision, &production_uri)? {
                    results.push(result);
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
    fn legacy_status_constants() {
        assert_eq!(legacy_statuses("status-any", false), Ok(None));
        assert_eq!(
            legacy_statuses("status-needs-review", false),
            Ok(Some(vec!["needs-review".to_owned(), "draft".to_owned()]))
        );
        assert!(matches!(
            legacy_statuses("bogus", false),
            Err(MethodError::Conduit { .. })
        ));
        assert_eq!(legacy_status("draft"), Some((0, "Draft")));
    }

    #[test]
    fn counts() {
        assert_eq!(count(None), Ok(0));
        assert_eq!(count(Some(&json!("3"))), Ok(3));
        assert!(count(Some(&json!(-1))).is_err());
    }
}
