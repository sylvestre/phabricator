//! `diffusion.repository.search`: `PhabricatorRepositorySearchEngine`
//! through `buildConduitResponse()`.
//!
//! Ported: the `callsigns`, `shortNames`, `types`, `almanacServicePHIDs`,
//! `ids` and `phids` constraints, every builtin order but `relevance`
//! (the default is `committed`, by the date of the latest commit), paging,
//! the fields and the `projects` attachment. Left to PHP: `uris`,
//! `projects` and `query` constraints, the `uris` and `metrics`
//! attachments, saved queries and order vectors.

use std::collections::HashMap;
use std::fmt::Write;

use phab_app_differential::repository::{self, Repository};
use phab_conduit::order::{self, Column, Nulls};
use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::{Database, DbError, edges, rows};
use serde_json::{Map, Value, json};

const QUERY_CLASS: &str = "PhabricatorRepositoryQuery";

/// `getValidConstraintKeys()` of the engine's fields.
const VALID_CONSTRAINTS: &[&str] = &[
    "callsigns",
    "shortNames",
    "types",
    "uris",
    "almanacServicePHIDs",
    "query",
    "ids",
    "phids",
    "projects",
];

const ATTACHMENTS: &[&str] = &["uris", "metrics", "projects"];

const ID: Column = Column::new("r.id");
const EPOCH: Column = Column::new("s.epoch").nulls(Nulls::Tail);
const SIZE: Column = Column::new("s.size").nulls(Nulls::Tail);
const NAME: Column = Column::new("r.name").ascending();
const CALLSIGN: Column = Column::new("r.callsign").ascending().nulls(Nulls::Tail);

/// The builtin orders of `PhabricatorRepositoryQuery`, in their order:
/// the first is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Committed,
    Name,
    Callsign,
    Size,
    Newest,
    Oldest,
}

impl Order {
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let key = match value {
            None | Some(Value::Null) => return Ok(Self::Committed),
            Some(Value::String(key)) => key.as_str(),
            Some(_) => return Err(MethodError::Unsupported("order vectors")),
        };
        match key {
            "committed" => Ok(Self::Committed),
            "name" => Ok(Self::Name),
            "callsign" => Ok(Self::Callsign),
            "size" => Ok(Self::Size),
            "newest" | "created" => Ok(Self::Newest),
            "oldest" => Ok(Self::Oldest),
            "relevance" => Err(MethodError::Unsupported("the relevance order")),
            other => Err(pager::core_error(format!(
                "Query \"{QUERY_CLASS}\" does not support a builtin order \"{other}\". \
                 Supported orders are: committed, name, callsign, size, newest, created, \
                 oldest, relevance."
            ))),
        }
    }

    /// The order vector's columns.
    fn columns(self) -> Vec<Column> {
        match self {
            Self::Committed => vec![EPOCH, ID],
            Self::Name => vec![NAME, ID],
            Self::Callsign => vec![CALLSIGN],
            Self::Size => vec![SIZE, ID],
            Self::Newest => vec![ID],
            Self::Oldest => vec![ID.reversed()],
        }
    }

    /// Whether the query joins the summary table
    /// (`shouldJoinSummaryTable()`).
    fn needs_summary(self) -> bool {
        matches!(self, Self::Committed | Self::Size)
    }
}

/// The ported constraints.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Filters {
    callsigns: Option<Vec<String>>,
    short_names: Option<Vec<String>>,
    types: Option<Vec<String>>,
    services: Option<Vec<String>>,
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
}

impl Filters {
    fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        // String lists and checkboxes take an empty list, which is no
        // constraint; PHID lists don't.
        let list = |key: &str, allow_empty: bool| -> Result<Option<Vec<String>>, MethodError> {
            Ok(constraints
                .get(&[key])
                .map(|(key, value)| search::string_list(key, value, allow_empty))
                .transpose()?
                .filter(|list| !list.is_empty()))
        };
        let mut filters = Self {
            callsigns: list("callsigns", true)?,
            short_names: list("shortNames", true)?,
            types: list("types", true)?,
            ..Self::default()
        };
        if list("uris", true)?.is_some() {
            return Err(MethodError::Unsupported("repository URI constraints"));
        }
        filters.services = list("almanacServicePHIDs", false)?;
        if let Some((key, value)) = constraints.get(&["query"])
            && !search::string(key, value)?.is_empty()
        {
            return Err(MethodError::Unsupported("fulltext repository search"));
        }
        if let Some((key, value)) = constraints.get(&["ids"]) {
            filters.ids = Some(search::int_list(key, value, strict)?);
        }
        filters.phids = list("phids", false)?;
        if constraints.has(&["projects"]) {
            return Err(MethodError::Unsupported("repository project constraints"));
        }
        Ok(filters)
    }

    fn add_conditions(&self, clauses: &mut Clauses) {
        if let Some(ids) = &self.ids {
            clauses.in_list("r.id", ids.iter().map(|id| json!(id)));
        }
        for (column, values) in [
            ("r.phid", &self.phids),
            ("r.callsign", &self.callsigns),
            ("r.versionControlSystem", &self.types),
            ("r.repositorySlug", &self.short_names),
            ("r.almanacServicePHID", &self.services),
        ] {
            if let Some(values) = values {
                clauses.in_list(column, search::strings(values));
            }
        }
    }
}

/// `PhabricatorRepositoryQuery` with the ported constraints.
struct Repositories<'a> {
    request: &'a ApiRequest<'a>,
    filters: Filters,
    order: Order,
}

impl Repositories<'_> {
    fn database(&self) -> &Database {
        &self.request.context.database
    }

    fn summary_join(&self) -> String {
        format!(
            " LEFT JOIN {} s ON r.id = s.repositoryID",
            self.database().table("repository", "repository_summary")
        )
    }

    /// `newPagingMapFromCursorObject()`: the values of the order's columns
    /// for the row with ID `id`.
    async fn paging_values(&self, id: u64) -> Result<Vec<Value>, MethodError> {
        let database = self.database();
        let sql = format!(
            "SELECT r.id, r.name, r.callsign, s.epoch, s.size FROM {} r{} WHERE r.id = ?",
            database.table("repository", "repository"),
            self.summary_join(),
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
            .ok_or_else(|| MethodError::internal("the cursor's repository vanished"))?;
        let int = |column: &str| -> Result<Value, MethodError> {
            Ok(rows::opt_int(&row, column)?.map_or(Value::Null, |value| json!(value)))
        };
        let text = |column: &str| -> Result<Value, MethodError> {
            Ok(rows::opt_text(&row, column)?.map_or(Value::Null, |value| json!(value)))
        };
        Ok(match self.order {
            Order::Committed => vec![int("epoch")?, int("id")?],
            Order::Name => vec![text("name")?, int("id")?],
            Order::Callsign => vec![text("callsign")?],
            Order::Size => vec![int("size")?, int("id")?],
            Order::Newest | Order::Oldest => vec![int("id")?],
        })
    }
}

impl PolicyAwareQuery for Repositories<'_> {
    type Row = Repository;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &Repository) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<Repository>, MethodError> {
        let database = self.database();
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("r.id", ids.iter().map(|id| json!(id)));
        } else {
            self.filters.add_conditions(&mut clauses);
        }
        let columns = self.order.columns();
        let reversed = direction == Direction::Backward;
        if let Some(after) = after {
            let values = self.paging_values(after).await?;
            if let Some((sql, binds)) = order::paging_clause(&columns, &values, reversed) {
                clauses.condition(sql, binds);
            }
        }
        let mut select = format!(
            "SELECT r.id FROM {} r",
            database.table("repository", "repository")
        );
        if self.order.needs_summary() {
            select.push_str(&self.summary_join());
        }
        let mut tail = order::order_by(&columns, reversed);
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(&select, &tail);
        let page: Vec<i64> = clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| rows::int(row, "id"))
            .collect::<Result<_, _>>()?;
        let mut loaded: HashMap<i64, Repository> = repository::load_by_ids(database, &page)
            .await?
            .into_iter()
            .map(|repository| (repository.id, repository))
            .collect();
        Ok(page.iter().filter_map(|id| loaded.remove(id)).collect())
    }

    async fn filter(&self, rows: Vec<Repository>) -> Result<Vec<Repository>, MethodError> {
        let context = self.request.context;
        repository::visible(
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

/// The columns the fields need beyond [`Repository`]: `dateCreated`,
/// `dateModified`, `almanacServicePHID` and the policies.
struct Columns {
    created: i64,
    modified: i64,
    service: Option<String>,
    policies: (String, String, String),
}

async fn load_columns(
    database: &Database,
    repositories: &[Repository],
) -> Result<HashMap<i64, Columns>, DbError> {
    if repositories.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, dateCreated, dateModified, almanacServicePHID, viewPolicy, editPolicy, \
         pushPolicy FROM {} WHERE id IN ({})",
        database.table("repository", "repository"),
        phab_db::placeholders(repositories.len())
    );
    phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        repositories.iter().map(|repository| repository.id),
    )
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(|row| {
        Ok((
            rows::int(row, "id")?,
            Columns {
                created: rows::int(row, "dateCreated")?,
                modified: rows::int(row, "dateModified")?,
                service: rows::opt_text(row, "almanacServicePHID")?,
                policies: (
                    rows::text(row, "viewPolicy")?,
                    rows::text(row, "editPolicy")?,
                    rows::text(row, "pushPolicy")?,
                ),
            },
        ))
    })
    .collect()
}

/// `getStringListForConduit()` of a detail which is a list.
fn string_list(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(list)) = value else {
        return match value {
            Some(Value::Object(map)) => map
                .values()
                .map(php_string)
                .filter(|value| !value.is_empty())
                .collect(),
            _ => Vec::new(),
        };
    };
    list.iter()
        .map(php_string)
        .filter(|value| !value.is_empty())
        .collect()
}

/// The keys of a detail which is a map (`array_keys()`), as strings.
fn keys(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Object(map)) => map.keys().filter(|key| !key.is_empty()).cloned().collect(),
        Some(Value::Array(list)) => (0..list.len()).map(|index| index.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// `(string)` of a scalar.
fn php_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "1".to_owned(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// One result: `getFieldValuesForConduit()` and the extensions' fields.
fn result(repository: &Repository, columns: &Columns, projects: Option<&[Phid]>) -> Value {
    let detail = |key: &str| repository.detail(key);
    let mut attachments = Map::new();
    if let Some(projects) = projects {
        attachments.insert(
            "projects".to_owned(),
            json!({"projectPHIDs": projects.iter().map(Phid::as_str).collect::<Vec<_>>()}),
        );
    }
    json!({
        "id": repository.id,
        "type": "REPO",
        "phid": repository.phid.as_str(),
        "fields": {
            "name": repository.name,
            "vcs": repository.vcs,
            "callsign": repository.callsign,
            "shortName": repository.slug,
            "status": if repository.is_tracked() { "active" } else { "inactive" },
            "isImporting": detail("importing").is_some_and(phab_core::php::is_truthy),
            "almanacServicePHID": columns.service,
            "refRules": {
                "fetchRules": string_list(detail("fetch-rules")),
                "trackRules": keys(detail("branch-filter")),
                "permanentRefRules": keys(detail("close-commits-filter")),
            },
            "defaultBranch": repository.default_branch(),
            "description": {
                "raw": detail("description").map(php_string).unwrap_or_default(),
            },
            "spacePHID": repository.subject().space.clone().flatten().as_ref().map(Phid::as_str),
            "dateCreated": columns.created,
            "dateModified": columns.modified,
            "policy": {
                "view": columns.policies.0,
                "edit": columns.policies.1,
                "diffusion.push": columns.policies.2,
            },
        },
        "attachments": attachments,
    })
}

/// `diffusion.repository.search`.
pub struct RepositorySearch;

impl Method for RepositorySearch {
    fn name(&self) -> &'static str {
        "diffusion.repository.search"
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
            if requested.iter().any(|key| *key != "projects") {
                return Err(MethodError::Unsupported("the uris and metrics attachments"));
            }
            let order = Order::parse(request.value("order"))?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let (start, direction) = paging.start();
            let query = Repositories {
                request,
                filters,
                order,
            };
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let database = &request.context.database;
            let columns = load_columns(database, &page.items).await?;
            let mut projects: HashMap<Phid, Vec<Phid>> = HashMap::new();
            if !requested.is_empty() {
                let phids: Vec<Phid> = page
                    .items
                    .iter()
                    .map(|repository| repository.phid.clone())
                    .collect();
                for edge in
                    edges::load_edges(database, &phids, &[edges::OBJECT_HAS_PROJECT], None).await?
                {
                    projects.entry(edge.src).or_default().push(edge.dst);
                }
            }
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|repository| {
                    let columns = columns
                        .get(&repository.id)
                        .ok_or_else(|| MethodError::internal("repository row vanished"))?;
                    let projects = (!requested.is_empty()).then(|| {
                        projects
                            .get(&repository.phid)
                            .map_or(&[][..], Vec::as_slice)
                    });
                    Ok(result(repository, columns, projects))
                })
                .collect::<Result<_, MethodError>>()?;
            Ok(search::response(
                request,
                &data,
                &paging,
                &page,
                |repository| repository.id.to_string(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders() {
        assert_eq!(Order::parse(None), Ok(Order::Committed));
        assert_eq!(Order::parse(Some(&json!("created"))), Ok(Order::Newest));
        assert!(matches!(
            Order::parse(Some(&json!("relevance"))),
            Err(MethodError::Unsupported(_))
        ));
        assert!(Order::parse(Some(&json!("nope"))).is_err());
        assert_eq!(
            order::order_by(&Order::Committed.columns(), false),
            " ORDER BY (s.epoch IS NOT NULL) DESC, s.epoch DESC, r.id DESC"
        );
    }

    #[test]
    fn ref_rules_like_php() {
        assert_eq!(string_list(Some(&json!(["a", "", 3]))), ["a", "3"]);
        assert_eq!(
            keys(Some(&json!({"default": true, "b": true}))),
            ["default", "b"]
        );
        assert_eq!(keys(None), Vec::<String>::new());
    }
}
