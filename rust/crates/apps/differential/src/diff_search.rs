//! `differential.diff.search`: `DifferentialDiffSearchEngine` through
//! `buildConduitResponse()`.
//!
//! Ported: every constraint (`revisionPHIDs`, `ids`, `phids`), the
//! `newest` and `oldest` orders, paging, the fields and the `commits`
//! attachment. Left to PHP: saved queries and order vectors.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints, IdOrder};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::{
    AutomaticCapability, Capability, ExtendedPolicy, ObjectKind, POLICY_PUBLIC, POLICY_USER,
    PolicySubject,
};
use serde_json::{Map, Value, json};

use crate::storage::{self, Repositories, Revision};

const QUERY_CLASS: &str = "DifferentialDiffQuery";

const VALID_CONSTRAINTS: &[&str] = &["revisionPHIDs", "ids", "phids"];

const ORDERS: &[&str] = &["newest", "created", "oldest"];

const ATTACHMENTS: &[&str] = &["commits"];

/// A `differential_diff` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffRow {
    pub(crate) id: i64,
    pub(crate) phid: Phid,
    pub(crate) revision_id: Option<i64>,
    pub(crate) author_phid: Option<String>,
    repository_phid: Option<String>,
    base: Option<String>,
    branch: Option<String>,
    bookmark: Option<String>,
    view_policy: String,
    date_created: i64,
    date_modified: i64,
}

const COLUMNS: &str = "id, phid, revisionID, authorPHID, repositoryPHID, \
     sourceControlBaseRevision, branch, bookmark, viewPolicy, dateCreated, dateModified";

impl DiffRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            revision_id: rows::opt_int(row, "revisionID")?,
            author_phid: rows::opt_text(row, "authorPHID")?,
            repository_phid: rows::opt_text(row, "repositoryPHID")?,
            base: rows::opt_text(row, "sourceControlBaseRevision")?,
            branch: rows::opt_text(row, "branch")?,
            bookmark: rows::opt_text(row, "bookmark")?,
            view_policy: rows::text(row, "viewPolicy")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
        })
    }

    /// A diff without a revision as a policy object: its view policy, its
    /// author, and its repository's view policy (`getExtendedPolicy()`).
    fn subject(&self) -> PolicySubject {
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other);
        subject.capabilities = vec![Capability::VIEW];
        subject = subject.with_policy(Capability::VIEW, self.view_policy.clone());
        if let Some(author) = &self.author_phid {
            subject.automatic.push(AutomaticCapability {
                principal: Phid::new(author.clone()),
                capabilities: None,
            });
        }
        if let Some(repository) = &self.repository_phid {
            subject.extended.push(ExtendedPolicy {
                for_capability: Capability::VIEW,
                object: Phid::new(repository.clone()),
                capabilities: vec![Capability::VIEW],
            });
        }
        subject
    }
}

/// `DifferentialDiffQuery` with the ported constraints.
struct Diffs<'a> {
    request: &'a ApiRequest<'a>,
    /// The IDs of the visible revisions of `revisionPHIDs`.
    revision_ids: Option<Vec<i64>>,
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
    order: IdOrder,
    /// The revisions of the diffs filtered so far, which the fields show.
    revisions: std::sync::Mutex<HashMap<i64, Revision>>,
}

impl PolicyAwareQuery for Diffs<'_> {
    type Row = DiffRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &DiffRow) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<DiffRow>, MethodError> {
        let database = &self.request.context.database;
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        } else {
            if let Some(ids) = &self.ids {
                clauses.in_list("id", ids.iter().map(|id| json!(id)));
            }
            if let Some(phids) = &self.phids {
                clauses.in_list("phid", search::strings(phids));
            }
            if let Some(revisions) = &self.revision_ids {
                clauses.in_list("revisionID", revisions.iter().map(|id| json!(id)));
            }
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
            &format!(
                "SELECT {COLUMNS} FROM {}",
                database.table("differential", "differential_diff")
            ),
            &tail,
        );
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(DiffRow::from_row)
            .collect::<Result<_, _>>()?)
    }

    /// `willFilterPage()` drops diffs whose revision the viewer can't see;
    /// the others follow their own policy.
    async fn filter(&self, rows: Vec<DiffRow>) -> Result<Vec<DiffRow>, MethodError> {
        let context = self.request.context;
        let database = &context.database;
        let viewer = &self.request.viewer;
        let mut revision_ids: Vec<i64> = rows.iter().filter_map(|row| row.revision_id).collect();
        revision_ids.sort_unstable();
        revision_ids.dedup();
        let revisions = storage::load_revisions_by_id(database, &revision_ids).await?;
        let revisions = storage::visible_revisions(
            database,
            &context.config,
            viewer,
            revisions,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        let visible_revisions: HashSet<i64> =
            revisions.iter().map(|revision| revision.id).collect();
        let rows: Vec<DiffRow> = rows
            .into_iter()
            .filter(|row| {
                row.revision_id
                    .is_none_or(|id| visible_revisions.contains(&id))
            })
            .collect();
        {
            let mut known = self
                .revisions
                .lock()
                .map_err(|_| MethodError::internal("revision map poisoned"))?;
            known.extend(
                revisions
                    .into_iter()
                    .map(|revision| (revision.id, revision)),
            );
        }
        // Diffs of a revision have the most open policy, which the
        // application check is all that is left of.
        let most_open = if context.config.allow_public() {
            POLICY_PUBLIC
        } else {
            POLICY_USER
        };
        let subjects: Vec<PolicySubject> = rows
            .iter()
            .map(|row| {
                if row.revision_id.is_some() {
                    let mut subject = PolicySubject::new(Some(row.phid.clone()), ObjectKind::Other);
                    subject.capabilities = vec![Capability::VIEW];
                    subject.with_policy(Capability::VIEW, most_open)
                } else {
                    row.subject()
                }
            })
            .collect();
        let refs: Vec<&PolicySubject> = subjects.iter().collect();
        let facts = phab_policy::loader::load_facts(
            database,
            &context.config,
            viewer,
            &refs,
            &Repositories { database },
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        let passed = phab_policy::filter(viewer, &refs, &[Capability::VIEW], &facts)
            .map_err(MethodError::internal)?;
        Ok(rows
            .into_iter()
            .zip(passed)
            .filter_map(|(row, visible)| visible.then_some(row))
            .collect())
    }
}

/// The diffs with these PHIDs the viewer can see, as `(ID, PHID)`
/// (`DifferentialDiffQuery::withPHIDs()`).
pub async fn visible_diffs(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<Vec<(i64, Phid)>, MethodError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let query = Diffs {
        request,
        revision_ids: None,
        ids: None,
        phids: Some(phids.iter().map(|phid| phid.as_str().to_owned()).collect()),
        order: IdOrder::Newest,
        revisions: std::sync::Mutex::default(),
    };
    let rows = query.load(None, None, Direction::Forward, None).await?;
    Ok(query
        .filter(rows)
        .await?
        .into_iter()
        .map(|row| (row.id, row.phid))
        .collect())
}

/// The diffs with these IDs the viewer can see
/// (`DifferentialDiffQuery::withIDs()`).
pub(crate) async fn visible_diff_rows(
    request: &ApiRequest<'_>,
    ids: &[i64],
) -> Result<Vec<DiffRow>, MethodError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    query_diffs(
        request,
        Some(ids.to_vec()),
        None,
        IdOrder::Newest,
        Direction::Forward,
    )
    .await
}

/// `DifferentialDiffQuery::execute()` without a limit: the diffs with
/// these IDs and of these revisions the viewer can see, in `order`.
pub(crate) async fn query_diffs(
    request: &ApiRequest<'_>,
    ids: Option<Vec<i64>>,
    revision_ids: Option<Vec<i64>>,
    order: IdOrder,
    direction: Direction,
) -> Result<Vec<DiffRow>, MethodError> {
    let query = Diffs {
        request,
        revision_ids,
        ids,
        phids: None,
        order,
        revisions: std::sync::Mutex::default(),
    };
    let rows = query.load(None, None, direction, None).await?;
    query.filter(rows).await
}

/// `DifferentialDiffQuery::withIDs(array($id))->executeOne()`: the diff,
/// or `None` when it doesn't exist or its revision is hidden
/// (`willFilterPage()` drops it). A hidden diff without a revision raises
/// a policy exception, which is left to PHP.
pub(crate) async fn execute_one(
    request: &ApiRequest<'_>,
    id: i64,
) -> Result<Option<DiffRow>, MethodError> {
    let found = query_diffs(
        request,
        Some(vec![id]),
        None,
        IdOrder::Newest,
        Direction::Forward,
    )
    .await?;
    if let Some(diff) = found.into_iter().next() {
        return Ok(Some(diff));
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT revisionID FROM {} WHERE id = ?",
        database.table("differential", "differential_diff")
    );
    if let Some(row) = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?
        && rows::opt_int(&row, "revisionID")?.unwrap_or(0) == 0
    {
        return Err(MethodError::Unsupported("a diff the viewer can't see"));
    }
    Ok(None)
}

/// A diff the viewer may see, as the web pages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleDiff {
    /// The row ID.
    pub id: i64,
    /// The diff PHID.
    pub phid: Phid,
    /// The revision it belongs to, if any.
    pub revision_id: Option<i64>,
    /// Who created it.
    pub author_phid: Option<String>,
    /// The repository, if known.
    pub repository_phid: Option<String>,
    /// `dateCreated`.
    pub date_created: i64,
}

impl From<DiffRow> for VisibleDiff {
    fn from(row: DiffRow) -> Self {
        Self {
            id: row.id,
            phid: row.phid,
            revision_id: row.revision_id,
            author_phid: row.author_phid,
            repository_phid: row.repository_phid,
            date_created: row.date_created,
        }
    }
}

/// What `DifferentialDiffQuery::withIDs(array($id))->executeOne()` finds
/// for a page's viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLookup {
    /// The viewer sees the diff.
    Visible(VisibleDiff),
    /// No such diff, or its revision is hidden (`willFilterPage()` drops
    /// it): a 404.
    NotFound,
    /// A diff without a revision the viewer may not see: the query raises
    /// a policy exception.
    Denied,
}

/// [`execute_one()`] for the web pages, which answer a hidden diff
/// without a revision with PHP's policy exception page.
pub async fn lookup(request: &ApiRequest<'_>, id: i64) -> Result<DiffLookup, MethodError> {
    match execute_one(request, id).await {
        Ok(Some(diff)) => Ok(DiffLookup::Visible(diff.into())),
        Ok(None) => Ok(DiffLookup::NotFound),
        Err(MethodError::Unsupported(_)) => Ok(DiffLookup::Denied),
        Err(error) => Err(error),
    }
}

/// The diffs of a revision the viewer may see, oldest first
/// (`DifferentialDiffQuery::withRevisionIDs()`, reversed).
pub async fn revision_diffs(
    request: &ApiRequest<'_>,
    revision_id: i64,
) -> Result<Vec<VisibleDiff>, MethodError> {
    let rows = query_diffs(
        request,
        None,
        Some(vec![revision_id]),
        IdOrder::Oldest,
        Direction::Forward,
    )
    .await?;
    Ok(rows.into_iter().map(VisibleDiff::from).collect())
}

/// The `arc:onto` and `local:commits` properties of diffs, by diff ID.
async fn load_properties(
    database: &Database,
    diffs: &[DiffRow],
    names: &[&str],
) -> Result<HashMap<(i64, String), Value>, DbError> {
    if diffs.is_empty() || names.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT diffID, name, data FROM {} WHERE diffID IN ({}) AND name IN ({})",
        database.table("differential", "differential_diffproperty"),
        phab_db::placeholders(diffs.len()),
        phab_db::placeholders(names.len()),
    );
    let query = phab_db::bind_all(phab_db::sqlx::query(&sql), diffs.iter().map(|diff| diff.id));
    phab_db::bind_all(query, names.iter().copied())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            let data = rows::opt_text(row, "data")?.unwrap_or_default();
            Ok((
                (rows::int(row, "diffID")?, rows::text(row, "name")?),
                serde_json::from_str(&data).unwrap_or(Value::Null),
            ))
        })
        .collect()
}

/// `loadTargetBranch()`: the branch of `arc:onto` when it names exactly
/// one target, which is a branch.
fn target_branch(onto: Option<&Value>) -> Option<Value> {
    let targets: Vec<&Value> = match onto? {
        Value::Array(list) => list.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => return None,
    };
    let [target] = targets.as_slice() else {
        return None;
    };
    let target = target.as_object()?;
    // PHP compares with `!=`: a missing type, null, is not "branch".
    if target.get("type").and_then(Value::as_str) != Some("branch") {
        return None;
    }
    target.get("name").cloned()
}

/// Whether a value is a nonempty string (`phutil_nonempty_string()`).
fn nonempty(value: Option<&String>) -> Option<&String> {
    value.filter(|value| !value.is_empty())
}

/// The value of a key of a commit, as `idx()` gives it.
fn idx<'v>(commit: &'v Value, key: &str) -> &'v Value {
    commit.get(key).unwrap_or(&Value::Null)
}

fn nonempty_value(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.is_empty(),
        Value::Null | Value::Bool(false) => false,
        _ => true,
    }
}

/// `addcslashes($name, '\\"')`, newlines folded, quoted: PHP's
/// `PhutilEmailAddress` display name.
fn quote_display_name(name: &str) -> String {
    let mut folded = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' || character == '\n' {
            // "\s*[\r\n]+\s*" becomes one space.
            while folded.ends_with(char::is_whitespace) {
                folded.pop();
            }
            while chars.peek().is_some_and(|next| next.is_whitespace()) {
                chars.next();
            }
            folded.push(' ');
        } else {
            folded.push(character);
        }
    }
    let mut quoted = String::from("\"");
    for character in folded.chars() {
        if character == '\\' || character == '"' {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// `DifferentialCommitsSearchEngineAttachment`: the `local:commits` of a
/// diff.
fn commits(local: Option<&Value>) -> Value {
    let commits: Vec<&Value> = match local {
        Some(Value::Object(map)) => map.values().collect(),
        Some(Value::Array(list)) => list.iter().collect(),
        _ => Vec::new(),
    };
    let list: Vec<Value> = commits
        .into_iter()
        .map(|commit| {
            let epoch = match idx(commit, "time") {
                value if !nonempty_value(value) || value == "0" => value.clone(),
                Value::Number(number) => json!(float_to_int(number)),
                Value::String(text) => json!(php_int(text)),
                other => other.clone(),
            };
            let name = idx(commit, "author");
            let email = idx(commit, "authorEmail");
            let raw = match (
                name.as_str().filter(|n| !n.is_empty()),
                email.as_str().filter(|e| !e.is_empty()),
            ) {
                (Some(name), Some(email)) => {
                    json!(format!("{} <{email}>", quote_display_name(name)))
                }
                (None, Some(_)) => email.clone(),
                _ => name.clone(),
            };
            json!({
                "identifier": idx(commit, "commit"),
                "tree": idx(commit, "tree"),
                "parents": commit.get("parents").cloned().unwrap_or_else(|| json!([])),
                "author": {"name": name, "email": email, "raw": raw, "epoch": epoch},
                "message": idx(commit, "message"),
                "firstPublicParent": idx(commit, "firstPublicParent"),
            })
        })
        .collect();
    json!({"commits": list})
}

/// `(int)` of a number: floats are truncated.
#[expect(clippy::cast_possible_truncation, reason = "PHP truncates floats too")]
fn float_to_int(number: &serde_json::Number) -> i64 {
    number
        .as_i64()
        .unwrap_or_else(|| number.as_f64().map_or(0, |value| value as i64))
}

/// `(int)` of a numeric string: its leading digits.
fn php_int(text: &str) -> i64 {
    let digits: String = text
        .trim_start()
        .chars()
        .enumerate()
        .take_while(|(index, character)| {
            character.is_ascii_digit() || (*index == 0 && *character == '-')
        })
        .map(|(_, character)| character)
        .collect();
    digits.parse().unwrap_or(0)
}

/// One result: `getFieldValuesForConduit()`, the dates and the policy,
/// and the `commits` attachment if asked for.
fn result(
    diff: &DiffRow,
    properties: &HashMap<(i64, String), Value>,
    revisions: &HashMap<i64, Revision>,
    most_open: &str,
    with_commits: bool,
) -> Value {
    let mut refs = Vec::new();
    if let Some(branch) = nonempty(diff.branch.as_ref()) {
        refs.push(json!({"type": "branch", "name": branch}));
    }
    let onto = target_branch(properties.get(&(diff.id, "arc:onto".to_owned())));
    if let Some(onto) = onto.filter(nonempty_value) {
        refs.push(json!({"type": "onto", "name": onto}));
    }
    if let Some(base) = diff.base.as_ref().filter(|base| !base.is_empty()) {
        refs.push(json!({"type": "base", "identifier": base}));
    }
    if let Some(bookmark) = nonempty(diff.bookmark.as_ref()) {
        refs.push(json!({"type": "bookmark", "name": bookmark}));
    }
    let revision_phid = diff
        .revision_id
        .and_then(|id| revisions.get(&id))
        .map(|revision| revision.phid.as_str());
    let view = if diff.revision_id.is_some() {
        most_open
    } else {
        diff.view_policy.as_str()
    };
    let mut attachments = Map::new();
    if with_commits {
        attachments.insert(
            "commits".to_owned(),
            commits(properties.get(&(diff.id, "local:commits".to_owned()))),
        );
    }
    json!({
        "id": diff.id,
        "type": "DIFF",
        "phid": diff.phid.as_str(),
        "fields": {
            "revisionPHID": revision_phid,
            "authorPHID": diff.author_phid,
            "repositoryPHID": diff.repository_phid,
            "refs": refs,
            "dateCreated": diff.date_created,
            "dateModified": diff.date_modified,
            "policy": {"view": view},
        },
        "attachments": attachments,
    })
}

/// `withRevisionPHIDs()`: the IDs of the revisions the viewer can see.
async fn visible_revision_ids(
    request: &ApiRequest<'_>,
    phids: &[String],
) -> Result<Vec<i64>, MethodError> {
    let database = &request.context.database;
    let phids: Vec<Phid> = phids.iter().cloned().map(Phid::new).collect();
    let revisions = storage::load_revisions(database, &phids).await?;
    let visible = storage::visible_revisions(
        database,
        &request.context.config,
        &request.viewer,
        revisions,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    Ok(visible.iter().map(|revision| revision.id).collect())
}

/// `differential.diff.search`.
pub struct DiffSearch;

impl Method for DiffSearch {
    fn name(&self) -> &'static str {
        "differential.diff.search"
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
            let revision_phids = constraints
                .get(&["revisionPHIDs"])
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
            let requested =
                search::requested_attachments(request.value("attachments"), ATTACHMENTS)?;
            let order = IdOrder::parse(request.value("order"), QUERY_CLASS, ORDERS)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let context = request.context;
            let database = &context.database;
            // No visible revision means no diffs.
            let revision_ids = match &revision_phids {
                Some(phids) => Some(visible_revision_ids(request, phids).await?),
                None => None,
            };
            let empty = revision_ids.as_ref().is_some_and(Vec::is_empty);
            let (start, direction) = paging.start();
            let query = Diffs {
                request,
                revision_ids,
                ids,
                phids,
                order,
                revisions: std::sync::Mutex::default(),
            };
            let found = if empty {
                Vec::new()
            } else {
                pager::execute(&query, start, direction, paging.size.value + 1).await?
            };
            let page = paging.pager().slice(found);
            let mut names = vec!["arc:onto"];
            if !requested.is_empty() {
                names.push("local:commits");
            }
            let properties = load_properties(database, &page.items, &names).await?;
            let revisions = query
                .revisions
                .lock()
                .map_err(|_| MethodError::internal("revision map poisoned"))?
                .clone();
            let most_open = if context.config.allow_public() {
                POLICY_PUBLIC
            } else {
                POLICY_USER
            };
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|diff| {
                    result(
                        diff,
                        &properties,
                        &revisions,
                        most_open,
                        !requested.is_empty(),
                    )
                })
                .collect();
            Ok(search::response(request, &data, &paging, &page, |diff| {
                diff.id.to_string()
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_like_php() {
        assert_eq!(
            target_branch(Some(&json!([{"type": "branch", "name": "main"}]))),
            Some(json!("main"))
        );
        assert_eq!(
            target_branch(Some(
                &json!([{"type": "branch", "name": "a"}, {"type": "branch"}])
            )),
            None
        );
        assert_eq!(
            target_branch(Some(&json!([{"type": "tag", "name": "a"}]))),
            None
        );
        assert_eq!(target_branch(None), None);
    }

    #[test]
    fn commits_like_php() {
        let local = json!({"abc": {
            "commit": "abc", "tree": "t", "parents": ["p"], "time": "1700000000",
            "author": "A \"B\"", "authorEmail": "a@b.c", "message": "m"
        }});
        assert_eq!(
            commits(Some(&local)),
            json!({"commits": [{
                "identifier": "abc", "tree": "t", "parents": ["p"],
                "author": {"name": "A \"B\"", "email": "a@b.c", "raw": "\"A \\\"B\\\"\" <a@b.c>", "epoch": 1_700_000_000},
                "message": "m", "firstPublicParent": null,
            }]})
        );
        assert_eq!(commits(None), json!({"commits": []}));
    }
}
