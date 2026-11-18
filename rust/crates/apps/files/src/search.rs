//! `file.search`: `PhabricatorFileSearchEngine` through
//! `buildConduitResponse()`.
//!
//! Ported: the `authorPHIDs`, `explicit`, `createdStart`, `createdEnd`,
//! `ids` and `phids` constraints, the `newest` and `oldest` orders, paging
//! and the fields. Visibility follows `PhabricatorFileQuery::loadPage()`
//! for files attached to revisions and repositories, and thumbnails of
//! visible files. Left to PHP: the `name` constraint (ngrams), files
//! attached to other objects or thumbnails of thumbnails, saved queries
//! and order vectors.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use phab_app_differential::{repository, storage};
use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints, IdOrder};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType};
use phab_db::{Database, rows};
use phab_policy::{POLICY_NOONE, POLICY_PUBLIC, POLICY_USER};
use serde_json::{Map, Value, json};

use crate::File;

/// `PhabricatorFilesApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorFilesApplication",
    name: "Files",
};

const QUERY_CLASS: &str = "PhabricatorFileQuery";

const VALID_CONSTRAINTS: &[&str] = &[
    "authorPHIDs",
    "explicit",
    "createdStart",
    "createdEnd",
    "name",
    "ids",
    "phids",
];

const ORDERS: &[&str] = &["newest", "created", "oldest"];

/// The ported constraints.
#[derive(Debug, Default)]
struct Filters {
    authors: Option<Vec<String>>,
    explicit: Option<bool>,
    created: (Option<i64>, Option<i64>),
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
}

impl Filters {
    fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        if constraints.has(&["name"]) {
            return Err(MethodError::Unsupported("file name searches"));
        }
        let mut filters = Self::default();
        if let Some((key, value)) = constraints.get(&["authorPHIDs"]) {
            let list = search::list(key, value, true)?;
            let authors = search::plain_phids(list, "PHID-USER-")?;
            filters.authors = Some(authors).filter(|authors| !authors.is_empty());
        }
        if let Some((key, value)) = constraints.get(&["explicit"]) {
            filters.explicit = Some(search::bool(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["createdStart"]) {
            filters.created.0 = Some(search::epoch(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["createdEnd"]) {
            filters.created.1 = Some(search::epoch(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["ids"]) {
            filters.ids = Some(search::int_list(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["phids"]) {
            filters.phids = Some(search::string_list(key, value, false)?);
        }
        Ok(filters)
    }

    fn add_conditions(&self, clauses: &mut Clauses) {
        // The engine's query never shows deleted files.
        clauses.condition("isDeleted = 0", []);
        if let Some(authors) = &self.authors {
            clauses.in_list("authorPHID", search::strings(authors));
        }
        if let Some(explicit) = self.explicit {
            clauses.condition("isExplicitUpload = ?", [json!(i64::from(explicit))]);
        }
        if let Some(start) = self.created.0 {
            clauses.condition("dateCreated >= ?", [json!(start)]);
        }
        if let Some(end) = self.created.1 {
            clauses.condition("dateCreated <= ?", [json!(end)]);
        }
        if let Some(ids) = &self.ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        }
        if let Some(phids) = &self.phids {
            clauses.in_list("phid", search::strings(phids));
        }
    }
}

/// The objects of these PHIDs the viewer can see: revisions and
/// repositories; other objects are left to PHP.
async fn visible_objects(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<HashSet<Phid>, MethodError> {
    let context = request.context;
    let database = &context.database;
    let of_type = |phid_type| -> Vec<Phid> {
        phids
            .iter()
            .filter(|phid| phid.phid_type() == phid_type)
            .cloned()
            .collect()
    };
    if phids
        .iter()
        .any(|phid| !matches!(phid.phid_type(), PhidType::REVISION | PhidType::REPOSITORY))
    {
        return Err(MethodError::Unsupported("files attached to other objects"));
    }
    let mut visible = HashSet::new();
    let revisions = storage::load_revisions(database, &of_type(PhidType::REVISION)).await?;
    for revision in storage::visible_revisions(
        database,
        &context.config,
        &request.viewer,
        revisions,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?
    {
        visible.insert(revision.phid);
    }
    let repositories = repository::load(database, &of_type(PhidType::REPOSITORY)).await?;
    for repo in repository::visible(
        database,
        &context.config,
        &request.viewer,
        repositories,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?
    {
        visible.insert(repo.phid);
    }
    Ok(visible)
}

/// The files the viewer can see among `files`, granted by an attached
/// object they can see, without following thumbnails.
async fn visible_by_attachment(
    request: &ApiRequest<'_>,
    files: Vec<File>,
) -> Result<Vec<File>, MethodError> {
    let database = &request.context.database;
    let attachments = crate::attached_objects(database, &files).await?;
    let mut objects: Vec<Phid> = attachments.values().flatten().cloned().collect();
    objects.sort();
    objects.dedup();
    let visible = visible_objects(request, &objects).await?;
    let granted: HashSet<Phid> = files
        .iter()
        .filter(|file| {
            attachments
                .get(&file.phid)
                .is_some_and(|objects| objects.iter().any(|phid| visible.contains(phid)))
        })
        .map(|file| file.phid.clone())
        .collect();
    crate::visible(
        database,
        &request.context.config,
        &request.viewer,
        files,
        &granted,
        request.now,
    )
    .await
    .map_err(MethodError::internal)
}

/// `PhabricatorFileQuery::loadPage()` and the policy filter: a file is
/// visible through its policy, its author, an attached object the viewer
/// can see, or its original when it is a thumbnail (which also needs the
/// original to be visible).
async fn visible_files(
    request: &ApiRequest<'_>,
    candidates: Vec<File>,
) -> Result<Vec<File>, MethodError> {
    let database = &request.context.database;
    let originals = crate::originals(database, &candidates).await?;
    let original_phids: Vec<Phid> = originals
        .values()
        .filter(|phid| phid.phid_type() != PhidType::VOID)
        .cloned()
        .collect();
    let original_files = crate::load_by_phids(database, &original_phids).await?;
    if !crate::originals(database, &original_files)
        .await?
        .is_empty()
    {
        return Err(MethodError::Unsupported("thumbnails of thumbnails"));
    }
    let visible_originals: HashSet<Phid> = visible_by_attachment(request, original_files)
        .await?
        .into_iter()
        .map(|file| file.phid)
        .collect();
    let mut kept = Vec::new();
    let mut thumbnails = HashSet::new();
    for file in candidates {
        match originals.get(&file.phid) {
            // PHID-VOID marks builtin thumbnails.
            Some(original) if original.phid_type() != PhidType::VOID => {
                if !visible_originals.contains(original) {
                    continue;
                }
                thumbnails.insert(file.phid.clone());
            }
            _ => {}
        }
        kept.push(file);
    }
    let attachments = crate::attached_objects(database, &kept).await?;
    let mut objects: Vec<Phid> = attachments.values().flatten().cloned().collect();
    objects.sort();
    objects.dedup();
    let visible = visible_objects(request, &objects).await?;
    let mut granted = thumbnails;
    for file in &kept {
        if attachments
            .get(&file.phid)
            .is_some_and(|objects| objects.iter().any(|phid| visible.contains(phid)))
        {
            granted.insert(file.phid.clone());
        }
    }
    crate::visible(
        database,
        &request.context.config,
        &request.viewer,
        kept,
        &granted,
        request.now,
    )
    .await
    .map_err(MethodError::internal)
}

/// `PhabricatorFileQuery` with the ported constraints.
struct Files<'a> {
    request: &'a ApiRequest<'a>,
    filters: Filters,
    order: IdOrder,
}

impl PolicyAwareQuery for Files<'_> {
    type Row = File;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &File) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<File>, MethodError> {
        let database = &self.request.context.database;
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        } else {
            self.filters.add_conditions(&mut clauses);
        }
        let (comparison, order) = search::id_paging(self.order, direction);
        if let Some(after) = after {
            clauses.condition(format!("id {comparison} ?"), [json!(after)]);
        }
        let mut tail = format!(" ORDER BY id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(
            &format!("SELECT id FROM {}", database.table("file", "file")),
            &tail,
        );
        let page: Vec<i64> = clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| rows::int(row, "id"))
            .collect::<Result<_, _>>()?;
        let mut loaded: HashMap<i64, File> = crate::load_by_ids(database, &page)
            .await?
            .into_iter()
            .map(|file| (file.id, file))
            .collect();
        Ok(page.iter().filter_map(|id| loaded.remove(id)).collect())
    }

    async fn filter(&self, rows: Vec<File>) -> Result<Vec<File>, MethodError> {
        visible_files(self.request, rows).await
    }
}

/// `dateCreated` and `dateModified` of files.
async fn load_dates(
    database: &Database,
    files: &[File],
) -> Result<HashMap<i64, (i64, i64)>, MethodError> {
    if files.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, dateCreated, dateModified FROM {} WHERE id IN ({})",
        database.table("file", "file"),
        phab_db::placeholders(files.len())
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), files.iter().map(|file| file.id))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                rows::int(row, "id")?,
                (
                    rows::int(row, "dateCreated")?,
                    rows::int(row, "dateModified")?,
                ),
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// One result: `getFieldValuesForConduit()` and the extensions' fields.
fn result(request: &ApiRequest<'_>, file: &File, dates: (i64, i64)) -> Result<Value, MethodError> {
    let config = &request.context.config;
    let most_open = if config.allow_public() {
        POLICY_PUBLIC
    } else {
        POLICY_USER
    };
    let view = if file.is_builtin() || file.metadata_truthy("profile") {
        most_open.to_owned()
    } else {
        file.view_policy.clone()
    };
    Ok(json!({
        "id": file.id,
        "type": "FILE",
        "phid": file.phid.as_str(),
        "fields": {
            "name": file.name,
            "uri": config
                .uri(&format!("/{}", file.monogram()))
                .map_err(MethodError::internal)?,
            "dataURI": file.view_uri(config),
            "size": file.byte_size,
            "alt": {
                "custom": file.custom_alt_text(),
                "default": file.default_alt_text(config),
            },
            "dateCreated": dates.0,
            "dateModified": dates.1,
            "policy": {"view": view, "edit": POLICY_NOONE},
        },
        "attachments": Map::new(),
    }))
}

/// `file.search`.
pub struct FileSearch;

impl Method for FileSearch {
    fn name(&self) -> &'static str {
        "file.search"
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
            search::check_query_key(request)?;
            let constraints = Constraints::parse(request.value("constraints"), VALID_CONSTRAINTS)?;
            let filters = Filters::parse(&constraints, request.strictly_typed)?;
            search::requested_attachments(request.value("attachments"), &[])?;
            let order = IdOrder::parse(request.value("order"), QUERY_CLASS, ORDERS)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let query = Files {
                request,
                filters,
                order,
            };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let dates = load_dates(&request.context.database, &page.items).await?;
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|file| {
                    let dates = dates
                        .get(&file.id)
                        .copied()
                        .ok_or_else(|| MethodError::internal("file row vanished"))?;
                    result(request, file, dates)
                })
                .collect::<Result<_, _>>()?;
            Ok(search::response(request, &data, &paging, &page, |file| {
                file.id.to_string()
            }))
        })
    }
}
