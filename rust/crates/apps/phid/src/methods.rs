//! `phid.query`, `phid.info` and `phid.lookup`
//! (`PHIDQueryConduitAPIMethod`, `PHIDInfoConduitAPIMethod`,
//! `PHIDLookupConduitAPIMethod`).
//!
//! Objects are users, revisions, diffs, projects and repositories; names
//! are PHIDs, `D123`, `@username` and `#hashtag`. Anything else is left to
//! PHP.

use std::collections::HashMap;

use phab_app_differential::storage;
use phab_app_project as project;
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::Phid;
use serde_json::{Map, Value};

use crate::handles::{self, Handle};

/// `PhabricatorEnv::getProductionURI()` of a path.
fn production_uri(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    let config = &request.context.config;
    match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => Ok(uri.to_owned()),
        None => config
            .base_uri()
            .map(str::to_owned)
            .map_err(MethodError::internal),
    }
}

/// A `list<string>` parameter; anything else is left to PHP.
fn string_list(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    match value {
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        _ => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// The values in their first order, without repeats (`array_unique()`).
fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = Vec::with_capacity(values.len());
    for value in values {
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    seen
}

/// A dictionary of results; PHP's empty array is a JSON list.
fn dictionary(map: Map<String, Value>) -> Value {
    if map.is_empty() {
        Value::Array(Vec::new())
    } else {
        Value::Object(map)
    }
}

/// `phid.query`.
pub struct PhidQuery;

impl Method for PhidQuery {
    fn name(&self) -> &'static str {
        "phid.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phids"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let phids: Vec<Phid> = unique(string_list(request.value("phids"))?)
                .into_iter()
                .map(Phid::new)
                .collect();
            let found = handles::load(request, &phids).await?;
            let base = production_uri(request)?;
            let mut result = Map::new();
            for phid in &phids {
                if let Some(handle) = found.get(phid) {
                    result.insert(phid.as_str().to_owned(), handle.dictionary(&base));
                }
            }
            Ok(dictionary(result))
        })
    }
}

/// `phid.info`.
pub struct PhidInfo;

impl Method for PhidInfo {
    fn name(&self) -> &'static str {
        "phid.info"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phid"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(Value::String(phid)) = request.value("phid") else {
                return Err(MethodError::Unsupported("an unusual PHID"));
            };
            let phid = Phid::new(phid.clone());
            let found = handles::load(request, std::slice::from_ref(&phid)).await?;
            let handle = found.get(&phid).ok_or_else(|| MethodError::Conduit {
                code: "ERR-BAD-PHID".to_owned(),
                info: Some("No such object exists.".to_owned()),
            })?;
            Ok(handle.dictionary(&production_uri(request)?))
        })
    }
}

/// `DifferentialRevisionPHIDType::canLoadNamedObject()`: the ID of `D123`.
fn revision_id(name: &str) -> Option<i64> {
    let digits = name.strip_prefix(['D', 'd'])?;
    let valid = !digits.is_empty()
        && !digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit());
    valid.then(|| digits.parse().ok()).flatten()
}

/// A hashtag `PhabricatorSlug::normalizeProjectSlug()` keeps as it is,
/// once lowercased: letters, digits and single inner `-` or `_`.
fn plain_slug(hashtag: &str) -> Option<String> {
    let slug = hashtag.to_lowercase();
    let separator = |c: char| c == '-' || c == '_';
    let plain = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || separator(c))
        && !slug.starts_with(separator)
        && !slug.ends_with(separator)
        && !slug.contains("--")
        && !slug.contains("__")
        && !slug.contains("-_")
        && !slug.contains("_-");
    plain.then_some(slug)
}

/// The objects named by `names` the viewer can see, by name
/// (`PhabricatorObjectQuery::withNames()` and `getNamedResults()`).
async fn named_objects(
    request: &ApiRequest<'_>,
    names: &[String],
) -> Result<HashMap<String, Phid>, MethodError> {
    let context = request.context;
    let database = &context.database;
    let mut found = HashMap::new();
    let mut phids = Vec::new();
    let mut revisions: Vec<(&String, i64)> = Vec::new();
    let mut users: Vec<(&String, String)> = Vec::new();
    let mut slugs: Vec<(&String, String)> = Vec::new();
    for name in names {
        if name.starts_with("PHID-") {
            phids.push(Phid::new(name.clone()));
        } else if let Some(id) = revision_id(name) {
            revisions.push((name, id));
        } else if let Some(user) = name.strip_prefix('@').filter(|user| !user.is_empty()) {
            users.push((name, user.to_lowercase()));
        } else if let Some(slug) = name.strip_prefix('#').and_then(plain_slug) {
            slugs.push((name, slug));
        } else {
            return Err(MethodError::Unsupported("objects of other names"));
        }
    }

    // Objects named by their PHIDs are those the handles load.
    for phid in handles::load(request, &phids).await?.into_keys() {
        found.insert(phid.as_str().to_owned(), phid);
    }

    let ids: Vec<i64> = revisions.iter().map(|(_, id)| *id).collect();
    if !ids.is_empty() {
        let loaded = storage::load_revisions_by_id(database, &ids).await?;
        let visible = storage::visible_revisions(
            database,
            &context.config,
            &request.viewer,
            loaded,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        for (name, id) in &revisions {
            if let Some(revision) = visible.iter().find(|revision| revision.id == *id) {
                found.insert((*name).clone(), revision.phid.clone());
            }
        }
    }

    if !users.is_empty() {
        let usernames: Vec<&str> = users.iter().map(|(_, user)| user.as_str()).collect();
        let loaded = phab_auth::user::load_users_by_username(database, &usernames).await?;
        for (name, user) in &users {
            if let Some(found_user) = loaded
                .iter()
                .find(|loaded| loaded.user_name.to_lowercase() == *user)
            {
                found.insert((*name).clone(), found_user.phid.clone());
            }
        }
    }

    if !slugs.is_empty() {
        let mut wanted: Vec<String> = slugs.iter().map(|(_, slug)| slug.clone()).collect();
        wanted.sort();
        wanted.dedup();
        let loaded = project::storage::load_by_slug(database, &wanted).await?;
        let candidates: Vec<project::Project> =
            loaded.iter().map(|(_, project)| project.clone()).collect();
        let (visible, _) = project::policy::visible_projects(
            database,
            &context.config,
            &request.viewer,
            candidates,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        for (name, slug) in &slugs {
            let project = loaded
                .iter()
                .find(|(found_slug, _)| found_slug == slug)
                .filter(|(_, project)| visible.iter().any(|v| v.phid == project.phid));
            if let Some((_, project)) = project {
                found.insert((*name).clone(), project.phid.clone());
            }
        }
    }
    Ok(found)
}

/// `phid.lookup`.
pub struct PhidLookup;

impl Method for PhidLookup {
    fn name(&self) -> &'static str {
        "phid.lookup"
    }

    fn params(&self) -> &'static [&'static str] {
        &["names"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let names = unique(string_list(request.value("names"))?);
            let objects = named_objects(request, &names).await?;
            let mut phids: Vec<Phid> = objects.values().cloned().collect();
            phids.sort();
            phids.dedup();
            let found: HashMap<Phid, Handle> = handles::load(request, &phids).await?;
            let base = production_uri(request)?;
            let mut result = Map::new();
            for name in &names {
                let handle = objects.get(name).and_then(|phid| found.get(phid));
                if let Some(handle) = handle {
                    result.insert(name.clone(), handle.dictionary(&base));
                }
            }
            Ok(dictionary(result))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_names() {
        assert_eq!(revision_id("D12"), Some(12));
        assert_eq!(revision_id("d3"), Some(3));
        assert_eq!(revision_id("D0"), None);
        assert_eq!(revision_id("D012"), None);
        assert_eq!(revision_id("D"), None);
    }

    #[test]
    fn plain_slugs() {
        assert_eq!(
            plain_slug("Release-Managers"),
            Some("release-managers".to_owned())
        );
        assert_eq!(plain_slug("a b"), None);
        assert_eq!(plain_slug("-a"), None);
        assert_eq!(plain_slug("a__b"), None);
    }

    #[test]
    fn unique_keeps_the_first_order() {
        let values = ["b", "a", "b"].map(str::to_owned).to_vec();
        assert_eq!(unique(values), ["b", "a"]);
    }
}
