//! `project.query` (`ProjectQueryConduitAPIMethod`): the legacy project
//! query, through the `project.search` query, by name.
//!
//! Ported: `ids`, `names`, `phids`, `slugs`, `colors`, `status`, `members`,
//! and `limit`; the method declares no cursors. Left to PHP: `icons` (PHP passes an empty list
//! to `withIcons()`), `offset` and values of unusual types.

use std::collections::HashMap;

use phab_conduit::pager::{self, PageSize, Paging};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::{Database, DbError, edges, rows};
use serde_json::{Map, Value, json};

use crate::Project;
use crate::search::{self, Filters, IconSet, Order, Projects};

/// A `list<string>` parameter; falsy is no constraint.
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

/// A `list<int>` parameter; falsy is no constraint.
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

/// `withStatus()`: the stored statuses of a legacy constant.
fn statuses(status: &str) -> Result<Option<Vec<String>>, MethodError> {
    match status {
        "status-any" => Ok(None),
        "status-open" | "status-active" => Ok(Some(vec!["0".to_owned()])),
        "status-closed" | "status-archived" => Ok(Some(vec!["100".to_owned()])),
        other => Err(pager::core_error(format!(
            "Unknown project status '{other}'!"
        ))),
    }
}

/// The constraints of the parameters, as `execute()` applies them.
fn filters(request: &ApiRequest<'_>) -> Result<Filters, MethodError> {
    for key in ["icons", "offset"] {
        if request.value(key).is_some_and(php::is_truthy) {
            return Err(MethodError::Unsupported(
                "a project.query parameter not ported",
            ));
        }
    }
    let mut filters = Filters {
        ids: id_list(request.value("ids"))?,
        names: string_list(request.value("names"))?,
        phids: string_list(request.value("phids"))?,
        slugs: string_list(request.value("slugs"))?.unwrap_or_default(),
        colors: string_list(request.value("colors"))?,
        members: string_list(request.value("members"))?,
        ..Filters::default()
    };
    match request.value("status") {
        None | Some(Value::Null) => {}
        Some(Value::String(status)) if status.is_empty() => {}
        Some(Value::String(status)) => filters.statuses = statuses(status)?,
        Some(_) => return Err(MethodError::Unsupported("an unusual status")),
    }
    Ok(filters)
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

/// What the results show beyond [`Project`].
struct Columns {
    profile_image: Option<String>,
    date_created: i64,
    date_modified: i64,
}

async fn load_columns(
    database: &Database,
    projects: &[Project],
) -> Result<HashMap<Phid, Columns>, DbError> {
    let sql = format!(
        "SELECT phid, profileImagePHID, dateCreated, dateModified FROM {} WHERE phid IN ({})",
        database.table("project", "project"),
        phab_db::placeholders(projects.len())
    );
    phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        projects.iter().map(|project| project.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(|row| {
        Ok((
            Phid::new(rows::text(row, "phid")?),
            Columns {
                profile_image: rows::opt_text(row, "profileImagePHID")?,
                date_created: rows::int(row, "dateCreated")?,
                date_modified: rows::int(row, "dateModified")?,
            },
        ))
    })
    .collect()
}

/// `needSlugs()`: every slug of each project, in ID order.
async fn load_slugs(
    database: &Database,
    projects: &[Project],
) -> Result<HashMap<Phid, Vec<String>>, DbError> {
    let sql = format!(
        "SELECT projectPHID, slug FROM {} WHERE projectPHID IN ({}) ORDER BY id",
        database.table("project", "project_slug"),
        phab_db::placeholders(projects.len())
    );
    let mut slugs: HashMap<Phid, Vec<String>> = HashMap::new();
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        projects.iter().map(|project| project.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    for row in &found {
        slugs
            .entry(Phid::new(rows::text(row, "projectPHID")?))
            .or_default()
            .push(rows::text(row, "slug")?);
    }
    Ok(slugs)
}

/// The members of each project: milestones have their parent's.
fn member_source(project: &Project) -> Phid {
    if project.is_milestone() {
        project
            .parent_phid
            .clone()
            .unwrap_or_else(|| project.phid.clone())
    } else {
        project.phid.clone()
    }
}

/// `buildProjectInfoDictionaries()` and the slug map.
async fn results(
    request: &ApiRequest<'_>,
    projects: &[Project],
    queried_slugs: &[String],
) -> Result<(Value, Value), MethodError> {
    if projects.is_empty() {
        return Ok((json!([]), json!([])));
    }
    let database = &request.context.database;
    let columns = load_columns(database, projects).await?;
    let slugs = load_slugs(database, projects).await?;
    let mut sources: Vec<Phid> = projects.iter().map(member_source).collect();
    sources.sort();
    sources.dedup();
    let members =
        search::load_edge_map(database, &sources, edges::PROJECT_MATERIALIZED_MEMBER).await?;
    let icons = IconSet::load(&request.context.config);
    let mut data = Map::new();
    for project in projects {
        let columns = columns
            .get(&project.phid)
            .ok_or_else(|| MethodError::internal("project row vanished"))?;
        let icon = if project.is_milestone() {
            icons.milestone_key()
        } else {
            json!(project.icon)
        };
        let member_phids: Vec<&str> = members
            .get(&member_source(project))
            .into_iter()
            .flatten()
            .map(Phid::as_str)
            .collect();
        data.insert(
            project.phid.as_str().to_owned(),
            json!({
                "id": project.id.to_string(),
                "phid": project.phid.as_str(),
                "name": project.name,
                "profileImagePHID": columns.profile_image,
                "icon": icon,
                "color": project.color,
                "members": member_phids,
                "slugs": slugs.get(&project.phid).cloned().unwrap_or_default(),
                "dateCreated": columns.date_created.to_string(),
                "dateModified": columns.date_modified.to_string(),
            }),
        );
    }
    // A slug names the last project with its normalized form.
    let mut slug_map = Map::new();
    for slug in queried_slugs {
        let Some(normal) = search::valid_slug(slug) else {
            continue;
        };
        for project in projects {
            if slugs
                .get(&project.phid)
                .is_some_and(|list| list.contains(&normal))
            {
                slug_map.insert(slug.clone(), json!(project.phid.as_str()));
            }
        }
    }
    let slug_map = if slug_map.is_empty() {
        json!([])
    } else {
        Value::Object(slug_map)
    };
    Ok((Value::Object(data), slug_map))
}

/// `project.query`.
pub struct LegacyQuery;

impl Method for LegacyQuery {
    fn name(&self) -> &'static str {
        "project.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "ids", "names", "phids", "slugs", "icons", "colors", "status", "members", "limit",
            "offset",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let filters = filters(request)?;
            let size = page_size(request.value("limit"))?;
            let paging = Paging {
                size: PageSize {
                    shown: json!(size),
                    value: size,
                },
                // The method declares no cursors: paging starts over.
                after: None,
                before: None,
            };
            let queried_slugs = filters.slugs.clone();
            let query = Projects {
                request,
                filters,
                order: Order::Name,
                ancestor_paths: Vec::new(),
            };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, size + 1).await?;
            let page = paging.pager().slice(found);
            let (data, slug_map) = results(request, &page.items, &queried_slugs).await?;
            let cursor = |project: Option<Project>| project.map(|project| project.id.to_string());
            Ok(json!({
                "data": data,
                "slugMap": slug_map,
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
    fn legacy_statuses() {
        assert_eq!(statuses("status-any"), Ok(None));
        assert_eq!(statuses("status-active"), Ok(Some(vec!["0".to_owned()])));
        assert!(statuses("bogus").is_err());
        assert_eq!(page_size(Some(&json!(5000))), Ok(1000));
        assert_eq!(page_size(None), Ok(100));
    }
}
