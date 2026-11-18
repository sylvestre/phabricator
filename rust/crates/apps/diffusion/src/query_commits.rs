//! `diffusion.querycommits` (`DiffusionQueryCommitsConduitAPIMethod`):
//! commits by ID, PHID or name, with their commit data, through
//! `diffusion.commit.search`'s query by ID.
//!
//! Ported: every parameter. Left to PHP: a `repositoryPHID` the viewer
//! can't see (`executeOne()` raises a policy exception), a page cut short
//! while `names` are mapped, and values of unusual types.

use std::collections::{HashMap, HashSet};

use phab_conduit::pager::{self, Cursor, PageSize, Paging};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::commit_search::{self, CommitRow, Commits, Filters, Order};

/// `PhabricatorRepository::MINIMUM_QUALIFIED_HASH`.
const MINIMUM_QUALIFIED_HASH: usize = 5;

/// A `list<...>` parameter; falsy is no constraint.
fn list(request: &ApiRequest<'_>, key: &str) -> Result<Option<Vec<Value>>, MethodError> {
    match request.value(key) {
        Some(Value::Array(list)) if !list.is_empty() => Ok(Some(list.clone())),
        Some(value) if php::is_truthy(value) => {
            Err(MethodError::Unsupported("lists which are not lists"))
        }
        _ => Ok(None),
    }
}

fn string_list(request: &ApiRequest<'_>, key: &str) -> Result<Option<Vec<String>>, MethodError> {
    list(request, key)?
        .map(|list| {
            list.iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or(MethodError::Unsupported(
                    "lists of values other than strings",
                ))
        })
        .transpose()
}

fn id_list(request: &ApiRequest<'_>) -> Result<Option<Vec<i64>>, MethodError> {
    list(request, "ids")?
        .map(|list| {
            list.iter()
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
                .ok_or(MethodError::Unsupported("IDs which are not integers"))
        })
        .transpose()
}

/// `ConduitAPIMethod::newPager()`: the limit, 100 by default, between 1
/// and 1000.
fn page_size(value: Option<&Value>) -> Result<usize, MethodError> {
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

/// `withRepository()` of a visible `repositoryPHID`; an unknown one is no
/// constraint.
async fn repository_filter(request: &ApiRequest<'_>) -> Result<Option<String>, MethodError> {
    let phid = match request.value("repositoryPHID") {
        Some(Value::String(phid)) if !phid.is_empty() && phid != "0" => phid,
        Some(value) if php::is_truthy(value) => {
            return Err(MethodError::Unsupported("an unusual repository PHID"));
        }
        _ => return Ok(None),
    };
    let context = request.context;
    let database = &context.database;
    let phids = [Phid::new(phid.clone())];
    let repositories = phab_app_differential::repository::load(database, &phids).await?;
    if repositories.is_empty() {
        return Ok(None);
    }
    let visible = phab_app_differential::repository::visible(
        database,
        &context.config,
        &request.viewer,
        repositories,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if visible.is_empty() {
        return Err(MethodError::Unsupported(
            "a repository the viewer can't see",
        ));
    }
    Ok(Some(phid.clone()))
}

/// The identifier map `willFilterPage()` builds from the commits it loads:
/// each name matching exactly one of them.
fn identifier_map(names: &[String], commits: &[CommitRow]) -> Map<String, Value> {
    let names: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut matches: HashMap<String, Vec<&Phid>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for commit in commits {
        let Some(repository) = &commit.repository else {
            continue;
        };
        let callsign = repository.callsign.as_deref().unwrap_or_default();
        let prefixes = [
            format!("r{callsign}"),
            format!("r{callsign}:"),
            format!("R{}:", repository.id),
            String::new(),
        ];
        for length in MINIMUM_QUALIFIED_HASH..=commit.identifier.len() {
            let Some(part) = commit.identifier.get(..length) else {
                continue;
            };
            for prefix in &prefixes {
                let key = format!("{prefix}{part}");
                if names.contains(key.as_str()) {
                    if !matches.contains_key(&key) {
                        order.push(key.clone());
                    }
                    matches.entry(key).or_default().push(&commit.phid);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|key| {
            let found = matches.get(&key)?;
            (found.len() == 1).then(|| (key, json!(found[0].as_str())))
        })
        .collect()
}

/// The `repository_commit` columns the results show beyond [`CommitRow`].
struct Columns {
    summary: String,
    author_phid: Option<String>,
    import_status: i64,
}

async fn load_columns(
    database: &Database,
    commits: &[CommitRow],
) -> Result<HashMap<i64, Columns>, MethodError> {
    if commits.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, summary, authorPHID, importStatus FROM {} WHERE id IN ({})",
        database.table("repository", "repository_commit"),
        phab_db::placeholders(commits.len())
    );
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        commits.iter().map(|commit| commit.id),
    )
    .fetch_all(database.pool())
    .await?;
    let mut columns = HashMap::new();
    for row in &found {
        columns.insert(
            rows::int(row, "id")?,
            Columns {
                summary: rows::text(row, "summary")?,
                author_phid: rows::opt_text(row, "authorPHID")?,
                import_status: rows::int(row, "importStatus")?,
            },
        );
    }
    Ok(columns)
}

/// A `repository_commitdata` row.
struct Data {
    author_name: Option<String>,
    message: Option<String>,
    details: Value,
}

async fn load_data(
    database: &Database,
    commits: &[CommitRow],
) -> Result<HashMap<i64, Data>, MethodError> {
    if commits.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT commitID, authorName, commitMessage, commitDetails FROM {} \
         WHERE commitID IN ({})",
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
        data.insert(
            rows::int(row, "commitID")?,
            Data {
                author_name: rows::opt_text(row, "authorName")?,
                message: rows::opt_text(row, "commitMessage")?,
                details: rows::json(row, "commitDetails")?,
            },
        );
    }
    Ok(data)
}

/// A string of the commit ref, as PHP would read it: `null` stays `null`,
/// strings are themselves and other values are left to PHP.
fn ref_string(value: Option<&Value>) -> Result<Option<String>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(MethodError::Unsupported("unusual commit details")),
    }
}

/// `getCommitDetailString()`: a nonempty string detail.
fn detail_string(details: &Value, key: &str) -> Result<Option<String>, MethodError> {
    Ok(ref_string(details.get(key))?.filter(|text| !text.is_empty()))
}

/// `DiffusionCommitRef::formatUser()`.
fn format_user(name: Option<&str>, email: Option<&str>) -> Option<String> {
    let (name, email) = (name.unwrap_or_default(), email.unwrap_or_default());
    match (name.is_empty(), email.is_empty()) {
        (false, false) => Some(format!("{name} <{email}>")),
        (_, false) => Some(email.to_owned()),
        (false, true) => Some(name.to_owned()),
        (true, true) => None,
    }
}

/// The people of `getCommitRef()`: the `ref` detail, completed by the
/// other details.
struct People {
    author_name: Option<String>,
    author_email: Option<String>,
    committer_name: Option<String>,
    committer_email: Option<String>,
}

impl People {
    fn read(details: &Value) -> Result<Self, MethodError> {
        let reference = match details.get("ref") {
            Some(Value::Object(map)) => Some(map),
            _ => None,
        };
        // The ref's keys win, even when they are null.
        let field = |key: &str| -> Result<Option<String>, MethodError> {
            match reference.filter(|map| map.contains_key(key)) {
                Some(map) => ref_string(map.get(key)),
                None => detail_string(details, key),
            }
        };
        Ok(Self {
            author_name: field("authorName")?,
            author_email: field("authorEmail")?,
            committer_name: field("committerName")?,
            committer_email: field("committerEmail")?,
        })
    }
}

/// One commit as `execute()` describes it.
fn result(
    commit: &CommitRow,
    columns: &Columns,
    data: Option<&Data>,
    production_uri: &str,
    need_messages: bool,
) -> Result<Value, MethodError> {
    let repository = commit
        .repository
        .as_ref()
        .ok_or_else(|| MethodError::internal("a commit without its repository"))?;
    let empty = json!({});
    let details = data.map_or(&empty, |data| &data.details);
    let people = People::read(details)?;
    let author = format_user(
        people.author_name.as_deref(),
        people.author_email.as_deref(),
    )
    .or_else(|| {
        data.and_then(|data| data.author_name.clone())
            .filter(|name| !name.is_empty())
    });
    let committer = match format_user(
        people.committer_name.as_deref(),
        people.committer_email.as_deref(),
    ) {
        Some(committer) => Some(committer),
        None => detail_string(details, "committer")?,
    };
    let mut dict = json!({
        // Lisk keeps MySQL's strings.
        "id": commit.id.to_string(),
        "phid": commit.phid.as_str(),
        "repositoryPHID": repository.phid.as_str(),
        "identifier": commit.identifier,
        "epoch": commit.epoch.to_string(),
        "authorEpoch": commit_search::author_epoch(details)?,
        "uri": format!(
            "{}{}",
            production_uri.trim_end_matches('/'),
            repository.commit_uri(&commit.identifier)
        ),
        "isImporting": columns.import_status & commit_search::IMPORTED_ALL
            != commit_search::IMPORTED_ALL,
        "summary": columns.summary,
        "authorPHID": columns.author_phid,
        "committerPHID": details.get("committerPHID").cloned().unwrap_or(Value::Null),
        "author": author,
        "authorName": people.author_name,
        "authorEmail": people.author_email,
        "committer": committer,
        "committerName": people.committer_name,
        "committerEmail": people.committer_email,
        "hashes": [],
    });
    if need_messages {
        dict["message"] = json!(data.and_then(|data| data.message.clone()));
    }
    Ok(dict)
}

/// `diffusion.querycommits`.
pub struct QueryCommits;

impl Method for QueryCommits {
    fn name(&self) -> &'static str {
        "diffusion.querycommits"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "ids",
            "phids",
            "names",
            "repositoryPHID",
            "needMessages",
            "bypassCache",
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
            let need_messages = request.value("needMessages").is_some_and(php::is_truthy);
            let names = string_list(request, "names")?;
            let filters = Filters {
                repositories: repository_filter(request).await?.map(|phid| vec![phid]),
                identifiers: names.clone(),
                ids: id_list(request)?,
                phids: string_list(request, "phids")?,
                ..Filters::default()
            };
            let size = page_size(request.value("limit"))?;
            let paging = Paging {
                size: PageSize {
                    shown: json!(size),
                    value: size,
                },
                after: Cursor::parse(request.value("after"))?,
                before: Cursor::parse(request.value("before"))?,
            };
            // getDefaultOrderVector(): by ID.
            let query = Commits::resolve(request, &filters, Order::ImportNew).await?;
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, size + 1).await?;
            let map = names
                .as_deref()
                .map(|names| identifier_map(names, &found))
                .unwrap_or_default();
            if names.is_some() && found.len() > size {
                return Err(MethodError::Unsupported(
                    "a page cut short while names are mapped",
                ));
            }
            let page = paging.pager().slice(found);
            let database = &request.context.database;
            let columns = load_columns(database, &page.items).await?;
            let data = load_data(database, &page.items).await?;
            let config = &request.context.config;
            let production_uri = match config.nonempty_string("phabricator.production-uri") {
                Some(uri) => uri.to_owned(),
                None => config
                    .base_uri()
                    .map(str::to_owned)
                    .map_err(MethodError::internal)?,
            };
            let mut results = Map::new();
            for commit in &page.items {
                let columns = columns
                    .get(&commit.id)
                    .ok_or_else(|| MethodError::internal("commit row vanished"))?;
                results.insert(
                    commit.phid.as_str().to_owned(),
                    result(
                        commit,
                        columns,
                        data.get(&commit.id),
                        &production_uri,
                        need_messages,
                    )?,
                );
            }
            let cursor = |commit: Option<CommitRow>| commit.map(|commit| commit.id.to_string());
            Ok(json!({
                "data": if results.is_empty() { json!([]) } else { Value::Object(results) },
                "identifierMap": map,
                "cursor": {
                    "limit": size,
                    "after": cursor(page.next),
                    "before": cursor(page.previous),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_users_like_php() {
        assert_eq!(
            format_user(Some("Alice"), Some("a@example.com")).as_deref(),
            Some("Alice <a@example.com>")
        );
        assert_eq!(
            format_user(None, Some("a@example.com")).as_deref(),
            Some("a@example.com")
        );
        assert_eq!(
            format_user(Some("Alice"), Some("")).as_deref(),
            Some("Alice")
        );
        assert_eq!(format_user(Some(""), None), None);
    }

    #[test]
    fn reads_the_commit_ref() {
        let people = People::read(&json!({
            "ref": {"authorName": "Ref", "committerName": null},
            "authorName": "Detail",
            "authorEmail": "d@example.com",
            "committerName": "C",
        }))
        .expect("people");
        assert_eq!(people.author_name.as_deref(), Some("Ref"));
        assert_eq!(people.author_email.as_deref(), Some("d@example.com"));
        assert_eq!(people.committer_name, None);
    }
}
