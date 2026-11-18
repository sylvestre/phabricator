//! `diffusion.commit.search`: `PhabricatorCommitSearchEngine` through
//! `buildConduitResponse()`.
//!
//! Ported: the `ids`, `phids`, `repositories`, `authors`, `statuses`,
//! `unreachable`, `permanent` and `identifiers` constraints (with plain
//! PHIDs), every builtin order but `relevance` (the default is `newest`,
//! by commit date), paging, the fields and the `auditors` attachment of
//! commits without audits. Left to PHP: `responsible`, `auditors`,
//! `ancestorsOf`, `query`, `subscribers` and `projects` constraints,
//! commits with audit requests, saved queries and order vectors.

use std::collections::HashMap;
use std::fmt::Write;

use phab_app_differential::commit;
use phab_app_differential::repository::{self, Repository};
use phab_conduit::order::{self, Column};
use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value, json};

const QUERY_CLASS: &str = "DiffusionCommitQuery";

/// The constraint keys of the engine's fields.
const VALID_CONSTRAINTS: &[&str] = &[
    "responsible",
    "authors",
    "auditors",
    "statuses",
    "repositories",
    "unreachable",
    "permanent",
    "ancestorsOf",
    "identifiers",
    "ids",
    "phids",
    "query",
    "subscribers",
    "projects",
];

const ATTACHMENTS: &[&str] = &["auditors"];

/// `PhabricatorRepositoryCommit::IMPORTED_ALL`.
pub(crate) const IMPORTED_ALL: i64 = 11;
/// `PhabricatorRepositoryCommit::IMPORTED_PERMANENT`.
const IMPORTED_PERMANENT: i64 = 1024;
/// `PhabricatorRepositoryCommit::IMPORTED_UNREACHABLE`.
const IMPORTED_UNREACHABLE: i64 = 2048;

const EPOCH: Column = Column::new("epoch");
const ID: Column = Column::new("id");

/// The builtin orders of `DiffusionCommitQuery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// By decreasing commit date, then ID.
    Newest,
    /// By increasing commit date, then ID.
    Oldest,
    /// By decreasing ID (`importnew`, `created`).
    ImportNew,
    /// By increasing ID.
    ImportOld,
}

impl Order {
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let key = match value {
            None | Some(Value::Null) => return Ok(Self::Newest),
            Some(Value::String(key)) => key.as_str(),
            Some(_) => return Err(MethodError::Unsupported("order vectors")),
        };
        match key {
            "newest" => Ok(Self::Newest),
            "oldest" => Ok(Self::Oldest),
            "importnew" | "created" => Ok(Self::ImportNew),
            "importold" => Ok(Self::ImportOld),
            "relevance" => Err(MethodError::Unsupported("the relevance order")),
            other => Err(pager::core_error(format!(
                "Query \"{QUERY_CLASS}\" does not support a builtin order \"{other}\". \
                 Supported orders are: newest, oldest, relevance, importnew, created, importold."
            ))),
        }
    }

    fn columns(self) -> Vec<Column> {
        match self {
            Self::Newest => vec![EPOCH, ID],
            Self::Oldest => vec![EPOCH.reversed(), ID.reversed()],
            Self::ImportNew => vec![ID],
            Self::ImportOld => vec![ID.reversed()],
        }
    }
}

/// The ported constraints.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Filters {
    pub(crate) ids: Option<Vec<i64>>,
    pub(crate) phids: Option<Vec<String>>,
    pub(crate) repositories: Option<Vec<String>>,
    pub(crate) authors: Option<Vec<String>>,
    pub(crate) statuses: Option<Vec<String>>,
    pub(crate) unreachable: Option<bool>,
    pub(crate) permanent: Option<bool>,
    pub(crate) identifiers: Option<Vec<String>>,
}

/// `DiffusionCommitAuditStatus::newModernKeys()`: legacy numbers.
fn modern_status(status: String) -> String {
    let modern = match status.as_str() {
        "0" => "none",
        "1" => "needs-audit",
        "2" => "concern-raised",
        "3" => "partially-audited",
        "4" => "audited",
        "5" => "needs-verification",
        _ => return status,
    };
    modern.to_owned()
}

impl Filters {
    fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        for key in ["responsible", "auditors", "subscribers", "projects"] {
            if constraints.has(&[key]) {
                return Err(MethodError::Unsupported("a commit constraint not ported"));
            }
        }
        // String lists and checkboxes take an empty list, which is no
        // constraint; PHID and datasource lists don't.
        let list = |key: &str, allow_empty: bool| -> Result<Option<Vec<String>>, MethodError> {
            Ok(constraints
                .get(&[key])
                .map(|(key, value)| search::string_list(key, value, allow_empty))
                .transpose()?
                .filter(|list| !list.is_empty()))
        };
        let flag = |key: &str| -> Result<Option<bool>, MethodError> {
            constraints
                .get(&[key])
                .map(|(key, value)| search::bool(key, value, strict))
                .transpose()
        };
        let mut filters = Self {
            authors: list("authors", true)?,
            statuses: list("statuses", true)?
                .map(|statuses| statuses.into_iter().map(modern_status).collect()),
            repositories: list("repositories", false)?,
            unreachable: flag("unreachable")?,
            permanent: flag("permanent")?,
            ..Self::default()
        };
        if list("ancestorsOf", true)?.is_some() {
            return Err(MethodError::Unsupported("commit ancestry"));
        }
        filters.identifiers = list("identifiers", true)?;
        if let Some((key, value)) = constraints.get(&["ids"]) {
            filters.ids = Some(search::int_list(key, value, strict)?);
        }
        filters.phids = list("phids", false)?;
        if let Some((key, value)) = constraints.get(&["query"])
            && !search::string(key, value)?.is_empty()
        {
            return Err(MethodError::Unsupported("fulltext commit search"));
        }
        // Usernames and functions are resolved by PHP.
        for values in [&filters.authors, &filters.repositories] {
            if values
                .iter()
                .flatten()
                .any(|value| !value.starts_with("PHID-"))
            {
                return Err(MethodError::Unsupported(
                    "constraint values which are not PHIDs",
                ));
            }
        }
        Ok(filters)
    }
}

/// `DiffusionCommitQuery` with the ported constraints, which
/// [`Commits::resolve()`] turned into conditions; `None` when nothing can
/// match (`PhabricatorEmptyQueryException`).
pub(crate) struct Commits<'a> {
    request: &'a ApiRequest<'a>,
    clauses: Option<Clauses>,
    order: Order,
}

impl<'a> Commits<'a> {
    pub(crate) async fn resolve(
        request: &'a ApiRequest<'a>,
        filters: &Filters,
        order: Order,
    ) -> Result<Self, MethodError> {
        let empty = Self {
            request,
            clauses: None,
            order,
        };
        let context = request.context;
        let database = &context.database;
        let mut clauses = Clauses::default();
        if let Some(phids) = &filters.repositories {
            let phids: Vec<Phid> = phids.iter().cloned().map(Phid::new).collect();
            let repositories = repository::load(database, &phids).await?;
            let visible = repository::visible(
                database,
                &context.config,
                &request.viewer,
                repositories,
                request.now,
            )
            .await
            .map_err(MethodError::internal)?;
            if visible.is_empty() {
                return Ok(empty);
            }
            clauses.in_list(
                "repositoryID",
                visible.iter().map(|repository| json!(repository.id)),
            );
        }
        if let Some(ids) = &filters.ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        }
        if let Some(phids) = &filters.phids {
            clauses.in_list("phid", search::strings(phids));
        }
        if let Some(authors) = &filters.authors {
            // selectPossibleAuthors(): only users write commits.
            let users: Vec<String> = authors
                .iter()
                .filter(|phid| phid.starts_with("PHID-USER-"))
                .cloned()
                .collect();
            if users.is_empty() {
                return Ok(empty);
            }
            clauses.in_list("authorPHID", search::strings(&users));
        }
        if let Some(identifiers) = &filters.identifiers {
            let condition = commit::identifier_condition(
                database,
                &context.config,
                &request.viewer,
                identifiers,
                request.now,
            )
            .await
            .map_err(MethodError::internal)?;
            let Some((sql, values)) = condition else {
                return Ok(empty);
            };
            clauses.condition(sql, values.into_iter().map(Value::String));
        }
        if let Some(statuses) = &filters.statuses {
            clauses.in_list("auditStatus", search::strings(statuses));
        }
        for (flag, value) in [
            (IMPORTED_UNREACHABLE, filters.unreachable),
            (IMPORTED_PERMANENT, filters.permanent),
        ] {
            match value {
                Some(true) => {
                    clauses.condition("(importStatus & ?) = ?", [json!(flag), json!(flag)]);
                }
                Some(false) => clauses.condition("(importStatus & ?) = 0", [json!(flag)]),
                None => {}
            }
        }
        Ok(Self {
            request,
            clauses: Some(clauses),
            order,
        })
    }

    fn database(&self) -> &Database {
        &self.request.context.database
    }

    /// The values of the order's columns for the commit with ID `id`.
    async fn paging_values(&self, id: u64) -> Result<Vec<Value>, MethodError> {
        let database = self.database();
        let sql = format!(
            "SELECT id, epoch FROM {} WHERE id = ?",
            database.table("repository", "repository_commit")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
            .ok_or_else(|| MethodError::internal("the cursor's commit vanished"))?;
        let id = json!(rows::int(&row, "id")?);
        Ok(match self.order {
            Order::Newest | Order::Oldest => vec![json!(rows::int(&row, "epoch")?), id],
            Order::ImportNew | Order::ImportOld => vec![id],
        })
    }
}

/// A `repository_commit` row, with its repository.
#[derive(Debug, Clone)]
pub(crate) struct CommitRow {
    pub(crate) id: i64,
    pub(crate) phid: Phid,
    repository_id: i64,
    pub(crate) identifier: String,
    pub(crate) epoch: i64,
    author_identity: Option<String>,
    committer_identity: Option<String>,
    audit_status: String,
    import_status: i64,
    pub(crate) repository: Option<Repository>,
}

const COLUMNS: &str = "id, phid, repositoryID, commitIdentifier, epoch, authorIdentityPHID, \
     committerIdentityPHID, auditStatus, importStatus";

impl CommitRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            repository_id: rows::int(row, "repositoryID")?,
            identifier: rows::text(row, "commitIdentifier")?,
            epoch: rows::int(row, "epoch")?,
            author_identity: rows::opt_text(row, "authorIdentityPHID")?,
            committer_identity: rows::opt_text(row, "committerIdentityPHID")?,
            audit_status: rows::text(row, "auditStatus")?,
            import_status: rows::int(row, "importStatus")?,
            repository: None,
        })
    }

    /// `isPartiallyImported()`.
    pub(crate) fn has_flags(&self, flags: i64) -> bool {
        self.import_status & flags == flags
    }
}

impl PolicyAwareQuery for Commits<'_> {
    type Row = CommitRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &CommitRow) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<CommitRow>, MethodError> {
        let database = self.database();
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
        let columns = self.order.columns();
        let reversed = direction == Direction::Backward;
        if let Some(after) = after {
            let values = self.paging_values(after).await?;
            if let Some((sql, binds)) = order::paging_clause(&columns, &values, reversed) {
                clauses.condition(sql, binds);
            }
        }
        let select = format!(
            "SELECT {COLUMNS} FROM {}",
            database.table("repository", "repository_commit")
        );
        let mut tail = order::order_by(&columns, reversed);
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(&select, &tail);
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(CommitRow::from_row)
            .collect::<Result<_, _>>()?)
    }

    /// `willFilterPage()`: commits follow their repository's policy.
    async fn filter(&self, rows: Vec<CommitRow>) -> Result<Vec<CommitRow>, MethodError> {
        let context = self.request.context;
        let database = &context.database;
        let mut ids: Vec<i64> = rows.iter().map(|row| row.repository_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let repositories = repository::load_by_ids(database, &ids).await?;
        let visible: HashMap<i64, Repository> = repository::visible(
            database,
            &context.config,
            &self.request.viewer,
            repositories,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?
        .into_iter()
        .map(|repository| (repository.id, repository))
        .collect();
        Ok(rows
            .into_iter()
            .filter_map(|mut row| {
                row.repository = Some(visible.get(&row.repository_id)?.clone());
                Some(row)
            })
            .collect())
    }
}

/// A `repository_identity`: who wrote or committed a commit.
#[derive(Debug, Clone)]
struct Identity {
    name: String,
    user: Option<String>,
}

impl Identity {
    /// `PhutilEmailAddress` of the name: its display name and address.
    fn parse(&self) -> (Option<String>, String) {
        let name = self.name.trim();
        if let Some(open) = name.rfind('<').filter(|_| name.ends_with('>')) {
            // /^(.*)<(.*?)>$/: the last "<", as ".*" is greedy.
            let display = name[..open].trim_matches(['\'', '"', ' ']).to_owned();
            let address = name[open + 1..name.len() - 1].to_owned();
            return (Some(display), address);
        }
        (None, name.to_owned())
    }

    fn result(identity: Option<(&String, &Self)>, epoch: Option<i64>) -> Value {
        let Some((phid, identity)) = identity else {
            return json!({
                "name": null, "email": null, "raw": null, "epoch": epoch,
                "identityPHID": null, "userPHID": null,
            });
        };
        let (name, email) = identity.parse();
        json!({
            "name": name,
            "email": email,
            "raw": identity.name,
            "epoch": epoch,
            "identityPHID": phid,
            "userPHID": identity.user,
        })
    }
}

async fn load_identities(
    database: &Database,
    phids: &[&String],
) -> Result<HashMap<String, Identity>, MethodError> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT phid, identityNameRaw, identityNameEncoding, currentEffectiveUserPHID FROM {} \
         WHERE phid IN ({})",
        database.table("repository", "repository_identity"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        phids.iter().map(|phid| phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    let mut identities = HashMap::new();
    for row in &found {
        let encoding = rows::opt_text(row, "identityNameEncoding")?;
        if encoding
            .as_deref()
            .is_some_and(|encoding| encoding != "utf8")
        {
            return Err(MethodError::Unsupported("identities in other encodings"));
        }
        let name = rows::text(row, "identityNameRaw")?;
        identities.insert(
            rows::text(row, "phid")?,
            Identity {
                name,
                user: rows::opt_text(row, "currentEffectiveUserPHID")?,
            },
        );
    }
    Ok(identities)
}

/// The message and author date of commits (`repository_commitdata`), by
/// commit ID.
async fn load_data(
    database: &Database,
    commits: &[CommitRow],
) -> Result<HashMap<i64, (String, Option<i64>)>, MethodError> {
    if commits.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT commitID, commitMessage, commitDetails FROM {} WHERE commitID IN ({})",
        database.table("repository", "repository_commitdata"),
        phab_db::placeholders(commits.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        commits.iter().map(|commit| commit.id),
    )
    .fetch_all(database.pool())
    .await?;
    let mut data = HashMap::new();
    for row in &found {
        let details = rows::json(row, "commitDetails")?;
        data.insert(
            rows::int(row, "commitID")?,
            (rows::text(row, "commitMessage")?, author_epoch(&details)?),
        );
    }
    Ok(data)
}

/// `PhabricatorRepositoryCommitData::getAuthorEpoch()`: from the commit
/// ref, or the details; falsy is `null`.
pub(crate) fn author_epoch(details: &Value) -> Result<Option<i64>, MethodError> {
    let value = match details.get("ref") {
        Some(Value::Object(reference)) if reference.contains_key("authorEpoch") => {
            reference.get("authorEpoch")
        }
        _ => details.get("authorEpoch"),
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) if !phab_core::php::is_truthy(value) => Ok(None),
        Some(Value::Number(number)) if !number.is_f64() => Ok(number.as_i64()),
        Some(Value::String(text)) if text.bytes().all(|b| b.is_ascii_digit()) => text
            .parse()
            .map(Some)
            .map_err(|_| MethodError::Unsupported("huge author dates")),
        Some(_) => Err(MethodError::Unsupported("unusual author dates")),
    }
}

/// `DiffusionCommitAuditStatus`: `(name, closed, ANSI color)`.
fn audit_status(status: &str) -> Option<(&'static str, bool, Option<&'static str>)> {
    Some(match status {
        "none" => ("No Audits", true, None),
        "needs-audit" => ("Audit Required", false, Some("magenta")),
        "concern-raised" => ("Concern Raised", false, Some("red")),
        "partially-audited" => ("Partially Audited", false, Some("yellow")),
        "audited" => ("Audited", true, Some("green")),
        "needs-verification" => ("Needs Verification", false, Some("magenta")),
        _ => return None,
    })
}

/// Whether any of these commits has audit requests.
async fn has_audits(database: &Database, commits: &[CommitRow]) -> Result<bool, MethodError> {
    if commits.is_empty() {
        return Ok(false);
    }
    let sql = format!(
        "SELECT 1 FROM {} WHERE commitPHID IN ({}) LIMIT 1",
        database.table("repository", "repository_auditrequest"),
        phab_db::placeholders(commits.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        commits.iter().map(|commit| commit.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    Ok(!found.is_empty())
}

/// The view policies of repositories, by ID.
async fn view_policies(
    database: &Database,
    commits: &[CommitRow],
) -> Result<HashMap<i64, String>, MethodError> {
    if commits.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, viewPolicy FROM {} WHERE id IN ({})",
        database.table("repository", "repository"),
        phab_db::placeholders(commits.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        commits.iter().map(|commit| commit.repository_id),
    )
    .fetch_all(database.pool())
    .await?;
    found
        .iter()
        .map(|row| Ok((rows::int(row, "id")?, rows::text(row, "viewPolicy")?)))
        .collect()
}

/// What the results need beyond the commit rows.
struct PageData {
    data: HashMap<i64, (String, Option<i64>)>,
    identities: HashMap<String, Identity>,
    policies: HashMap<i64, String>,
    auditors: bool,
}

impl PageData {
    async fn load(
        database: &Database,
        commits: &[CommitRow],
        auditors: bool,
    ) -> Result<Self, MethodError> {
        // Audit requests are not ported; their status and the auditors
        // attachment would show them.
        if has_audits(database, commits).await?
            || commits.iter().any(|commit| commit.audit_status != "none")
        {
            return Err(MethodError::Unsupported("commits with audits"));
        }
        let identity_phids: Vec<&String> = commits
            .iter()
            .flat_map(|commit| [&commit.author_identity, &commit.committer_identity])
            .flatten()
            .collect();
        Ok(Self {
            data: load_data(database, commits).await?,
            identities: load_identities(database, &identity_phids).await?,
            policies: view_policies(database, commits).await?,
            auditors,
        })
    }

    /// The identity with this PHID, with its PHID.
    fn identity<'s>(&'s self, phid: Option<&'s String>) -> Option<(&'s String, &'s Identity)> {
        phid.and_then(|phid| Some((phid, self.identities.get(phid)?)))
    }

    /// One result: `getFieldValuesForConduit()` and the extensions' fields.
    fn result(&self, commit: &CommitRow) -> Result<Value, MethodError> {
        let (message, author_epoch) = self
            .data
            .get(&commit.id)
            .map_or((None, None), |(message, epoch)| (Some(message), *epoch));
        let (name, closed, ansi) = audit_status(&commit.audit_status)
            .ok_or(MethodError::Unsupported("an unknown audit status"))?;
        let repository = commit
            .repository
            .as_ref()
            .ok_or_else(|| MethodError::internal("commit without its repository"))?;
        let view = self
            .policies
            .get(&repository.id)
            .ok_or_else(|| MethodError::internal("repository row vanished"))?;
        let mut attachments = Map::new();
        if self.auditors {
            attachments.insert("auditors".to_owned(), json!({"auditors": []}));
        }
        Ok(json!({
            "id": commit.id,
            "type": "CMIT",
            "phid": commit.phid.as_str(),
            "fields": {
                "identifier": commit.identifier,
                "repositoryPHID": repository.phid.as_str(),
                "author": Identity::result(self.identity(commit.author_identity.as_ref()), author_epoch),
                "committer": Identity::result(self.identity(commit.committer_identity.as_ref()), Some(commit.epoch)),
                "isUnreachable": commit.has_flags(IMPORTED_UNREACHABLE),
                "isImported": commit.has_flags(IMPORTED_ALL),
                "auditStatus": {
                    "value": commit.audit_status,
                    "name": name,
                    "closed": closed,
                    "color.ansi": ansi,
                },
                "message": message,
                "policy": {"view": view, "edit": "users"},
            },
            "attachments": attachments,
        }))
    }
}

/// `diffusion.commit.search`.
pub struct CommitSearch;

impl Method for CommitSearch {
    fn name(&self) -> &'static str {
        "diffusion.commit.search"
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
            let order = Order::parse(request.value("order"))?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let (start, direction) = paging.start();
            let query = Commits::resolve(request, &filters, order).await?;
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let page_data = PageData::load(
                &request.context.database,
                &page.items,
                !requested.is_empty(),
            )
            .await?;
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|commit| page_data.result(commit))
                .collect::<Result<_, _>>()?;
            Ok(search::response(request, &data, &paging, &page, |commit| {
                commit.id.to_string()
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders() {
        assert_eq!(Order::parse(None), Ok(Order::Newest));
        assert_eq!(Order::parse(Some(&json!("created"))), Ok(Order::ImportNew));
        assert!(matches!(
            Order::parse(Some(&json!("relevance"))),
            Err(MethodError::Unsupported(_))
        ));
        assert!(Order::parse(Some(&json!("nope"))).is_err());
    }

    #[test]
    fn identities_split_like_php() {
        let parse = |name: &str| {
            Identity {
                name: name.to_owned(),
                user: None,
            }
            .parse()
        };
        assert_eq!(
            parse("Bob Builder <bob@example.com>"),
            (Some("Bob Builder".to_owned()), "bob@example.com".to_owned())
        );
        assert_eq!(
            parse("\"Q\" <q@x>"),
            (Some("Q".to_owned()), "q@x".to_owned())
        );
        assert_eq!(
            parse("bob@example.com"),
            (None, "bob@example.com".to_owned())
        );
        assert_eq!(parse("bob"), (None, "bob".to_owned()));
    }

    #[test]
    fn author_dates() {
        assert_eq!(
            author_epoch(&json!({"ref": {"authorEpoch": null}, "authorEpoch": "5"})),
            Ok(None)
        );
        assert_eq!(author_epoch(&json!({"authorEpoch": "5"})), Ok(Some(5)));
        assert_eq!(
            author_epoch(&json!({"ref": {"authorEpoch": 7}})),
            Ok(Some(7))
        );
        assert_eq!(author_epoch(&json!({"authorEpoch": "0"})), Ok(None));
    }

    #[test]
    fn legacy_statuses() {
        assert_eq!(modern_status("1".to_owned()), "needs-audit");
        assert_eq!(modern_status("audited".to_owned()), "audited");
    }
}
