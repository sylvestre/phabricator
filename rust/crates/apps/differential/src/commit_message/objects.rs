//! The users and projects of commit message fields: how
//! `renderHandleList()` names them and how `PhabricatorObjectListQuery`
//! finds them by name. Other objects are left to PHP.

use std::collections::{HashMap, HashSet};

use phab_app_project::storage::Project;
use phab_conduit::{ApiRequest, MethodError};
use phab_core::phid::phid_type_of;
use phab_core::{Phid, PhidType};
use phab_db::rows;

/// The users among `phids` who exist, with their usernames.
async fn usernames_by_phid(
    request: &ApiRequest<'_>,
    phids: &[&str],
) -> Result<HashMap<String, String>, MethodError> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT phid, userName FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(phids.len())
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().copied())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| Ok((rows::text(row, "phid")?, rows::text(row, "userName")?)))
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// `PhabricatorPeopleQuery::withUsernames()`, matched back the way
/// `loadObjects()` and `PhabricatorPeopleUserPHIDType::loadNamedObjects()`
/// do: by lowercase username. MySQL compares `userName` without case, so
/// lowercase names find the same users.
async fn users_by_lowercase_name(
    request: &ApiRequest<'_>,
    names: &[String],
) -> Result<HashMap<String, String>, MethodError> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT phid, userName FROM {} WHERE userName IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(names.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), names.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?;
    let mut users = HashMap::new();
    for row in &rows {
        users.insert(
            rows::text(row, "userName")?.to_lowercase(),
            rows::text(row, "phid")?,
        );
    }
    Ok(users)
}

/// The projects the viewer can see among `projects`.
async fn visible(
    request: &ApiRequest<'_>,
    projects: Vec<Project>,
) -> Result<Vec<Project>, MethodError> {
    if projects.is_empty() {
        return Ok(projects);
    }
    let context = request.context;
    let (visible, _) = phab_app_project::policy::visible_projects(
        &context.database,
        &context.config,
        &request.viewer,
        projects,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    Ok(visible)
}

/// `renderHandleList()`: how a commit message names each of `phids`
/// (`getCommandLineObjectName()`), in order and without duplicates. A user
/// is named by username, a project by hashtag (by PHID without one), and
/// a project the viewer can't see by PHID. Objects which don't exist (whose
/// token PHP leaves undefined) and objects of other types are left to PHP.
pub(super) async fn command_line_names(
    request: &ApiRequest<'_>,
    phids: &[String],
) -> Result<Vec<String>, MethodError> {
    let mut unique: Vec<&str> = Vec::new();
    for phid in phids {
        if !unique.contains(&phid.as_str()) {
            unique.push(phid);
        }
    }
    let (users, projects): (Vec<&str>, Vec<&str>) = unique
        .iter()
        .partition(|phid| phid_type_of(phid) == PhidType::USER);
    if projects
        .iter()
        .any(|phid| phid_type_of(phid) != PhidType::PROJECT)
    {
        return Err(MethodError::Unsupported(
            "commit message fields with objects other than users and projects",
        ));
    }
    let usernames = usernames_by_phid(request, &users).await?;
    let wanted: Vec<Phid> = projects.iter().map(|phid| Phid::new(*phid)).collect();
    let loaded =
        phab_app_project::storage::load_by_phid(&request.context.database, &wanted).await?;
    let existing: HashSet<String> = loaded
        .iter()
        .map(|project| project.phid.as_str().to_owned())
        .collect();
    let visible: HashMap<String, Project> = visible(request, loaded)
        .await?
        .into_iter()
        .map(|project| (project.phid.as_str().to_owned(), project))
        .collect();
    let missing = || MethodError::Unsupported("commit message fields with unknown objects");
    unique
        .iter()
        .map(|phid| {
            if phid_type_of(phid) == PhidType::USER {
                return usernames.get(*phid).cloned().ok_or_else(missing);
            }
            if !existing.contains(*phid) {
                return Err(missing());
            }
            Ok(match visible.get(*phid) {
                Some(project) => match project.primary_slug.as_deref() {
                    Some(slug) if !slug.is_empty() => format!("#{slug}"),
                    _ => (*phid).to_owned(),
                },
                // A policy-filtered handle.
                None => (*phid).to_owned(),
            })
        })
        .collect()
}

/// An object of a parsed list, with whether it had the `!` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub(super) phid: String,
    pub(super) blocking: bool,
}

/// `$map[$key] = $value` on a PHP array: an existing key keeps its place.
fn put<V>(map: &mut Vec<(String, V)>, key: &str, value: V) {
    match map.iter_mut().find(|(known, _)| known == key) {
        Some(entry) => entry.1 = value,
        None => map.push((key.to_owned(), value)),
    }
}

/// How `PhabricatorObjectQuery` would look a name up.
enum Lookup {
    /// A PHID of a user or project.
    Phid,
    /// `@username`, by lowercase username.
    User(String),
    /// `#hashtag`, by lowercase hashtag.
    Project(String),
    /// No PHID type loads it by name.
    Nothing,
}

/// Names which `canLoadNamedObject()` of a type other than users and
/// projects may accept: revisions, commits, repositories, files and so on
/// (a superset of their patterns).
fn may_name_other_object(name: &str) -> bool {
    let bytes = name.as_bytes();
    let digits_after =
        |start: usize| bytes.len() > start && bytes[start..].iter().all(u8::is_ascii_digit);
    match bytes.first() {
        Some(
            b'B' | b'b' | b'W' | b'w' | b'S' | b's' | b'E' | b'e' | b'D' | b'd' | b'H' | b'h'
            | b'K' | b'k' | b'F',
        ) if digits_after(1) => {
            return true;
        }
        Some(b'R') if bytes.get(1).is_some_and(u8::is_ascii_digit) => return true,
        Some(b'r') if bytes.get(1).is_some_and(u8::is_ascii_uppercase) => return true,
        _ => {}
    }
    (7..=40).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn lookup(name: &str) -> Result<Lookup, MethodError> {
    if name.starts_with("PHID-") {
        let phid_type = phid_type_of(name);
        return match phid_type {
            PhidType::USER | PhidType::PROJECT => Ok(Lookup::Phid),
            // No type loads it.
            PhidType::UNKNOWN => Ok(Lookup::Nothing),
            _ => Err(MethodError::Unsupported(
                "commit message fields with PHIDs of other objects",
            )),
        };
    }
    if may_name_other_object(name) {
        return Err(MethodError::Unsupported(
            "commit message fields naming objects other than users and projects",
        ));
    }
    if let Some(rest) = name.strip_prefix('@')
        && !rest.is_empty()
    {
        return Ok(Lookup::User(rest.to_ascii_lowercase()));
    }
    if let Some(rest) = name.strip_prefix('#')
        && !rest.is_empty()
        && !rest.contains('#')
    {
        return Ok(Lookup::Project(rest.to_ascii_lowercase()));
    }
    Ok(Lookup::Nothing)
}

/// Whether PHP would turn `key` into an integer array key.
fn is_integer_key(key: &str) -> bool {
    let digits = key.strip_prefix('-').unwrap_or(key);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// The names of a list, in the order of the PHP array, with whether they
/// had the `!` suffix (the last of the same name decides) when `suffixes`.
fn list_names(value: &str, suffixes: bool) -> Result<Vec<(String, bool)>, MethodError> {
    // Printable ASCII and whitespace only: `\s` and trim() agree on them,
    // and lowercasing is ASCII.
    if value
        .bytes()
        .any(|byte| !(byte.is_ascii_graphic() || b" \t\n\x0B\x0C".contains(&byte)))
    {
        return Err(MethodError::Unsupported(
            "commit message lists with unusual characters",
        ));
    }
    let normalized = value
        .split(|c: char| c.is_ascii_whitespace() || c == '\x0B')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // A leading or trailing run of whitespace is one space, which trim()
    // removes from the names anyway.
    let mut names: Vec<&str> = Vec::new();
    for name in normalized.split(',').map(|name| name.trim_matches(' ')) {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    let mut name_map: Vec<(String, ())> = Vec::new();
    for name in &names {
        let parts: Vec<&str> = name.split(' ').collect();
        let first = parts[0].as_bytes();
        if first.len() > 1
            && first[0].is_ascii_uppercase()
            && first[1..].iter().all(u8::is_ascii_digit)
        {
            return Err(MethodError::Unsupported(
                "commit message lists with monograms",
            ));
        }
        for part in parts {
            put(&mut name_map, part, ());
        }
    }
    let mut stripped: Vec<(String, bool)> = Vec::new();
    for (name, ()) in &name_map {
        if is_integer_key(name) {
            return Err(MethodError::Unsupported(
                "commit message lists with numbers",
            ));
        }
        let key = if suffixes {
            name.trim_end_matches('!')
        } else {
            name.as_str()
        };
        if key.is_empty() || is_integer_key(key) {
            return Err(MethodError::Unsupported(
                "commit message lists with odd names",
            ));
        }
        put(&mut stripped, key, key.len() != name.len());
    }
    Ok(stripped)
}

/// `loadObjects()`: the objects found, by name, in its order: those
/// `PhabricatorObjectQuery` finds by PHID and by name (`@user`,
/// `#hashtag`), then users by username.
async fn load_objects(
    request: &ApiRequest<'_>,
    stripped: &[(String, bool)],
) -> Result<Vec<(String, String)>, MethodError> {
    let lookups: Vec<(&str, Lookup)> = stripped
        .iter()
        .map(|(name, _)| Ok((name.as_str(), lookup(name)?)))
        .collect::<Result<_, MethodError>>()?;
    let by_phid: Vec<&str> = lookups
        .iter()
        .filter(|(_, lookup)| matches!(lookup, Lookup::Phid))
        .map(|(name, _)| *name)
        .collect();
    let (user_phids, project_phids): (Vec<&str>, Vec<&str>) = by_phid
        .iter()
        .partition(|phid| phid_type_of(phid) == PhidType::USER);
    let known_users = usernames_by_phid(request, &user_phids).await?;
    let database = &request.context.database;
    let wanted: Vec<Phid> = project_phids.iter().map(|phid| Phid::new(*phid)).collect();
    let mut projects = phab_app_project::storage::load_by_phid(database, &wanted).await?;
    let slugs: Vec<String> = lookups
        .iter()
        .filter_map(|(_, lookup)| match lookup {
            Lookup::Project(slug) => Some(slug.clone()),
            _ => None,
        })
        .collect();
    let mut slug_projects: Vec<(String, Phid)> = Vec::new();
    for (slug, project) in phab_app_project::storage::load_by_slug(database, &slugs).await? {
        // The slug column is binary, but compare exactly anyway.
        if slugs.contains(&slug) {
            slug_projects.push((slug, project.phid.clone()));
            if !projects.iter().any(|known| known.phid == project.phid) {
                projects.push(project);
            }
        }
    }
    let visible_projects: HashSet<Phid> = visible(request, projects)
        .await?
        .into_iter()
        .map(|project| project.phid)
        .collect();
    let at_names: Vec<String> = lookups
        .iter()
        .filter_map(|(_, lookup)| match lookup {
            Lookup::User(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    let at_users = users_by_lowercase_name(request, &at_names).await?;

    // The objects found, by name, in the order loadObjects() returns them.
    let mut objects: Vec<(String, String)> = Vec::new();
    for (name, lookup) in &lookups {
        let found = match lookup {
            Lookup::Phid if phid_type_of(name) == PhidType::USER => {
                known_users.contains_key(*name).then(|| (*name).to_owned())
            }
            Lookup::Phid => visible_projects
                .contains(&Phid::new(*name))
                .then(|| (*name).to_owned()),
            Lookup::User(lower) => at_users.get(lower).cloned(),
            Lookup::Project(slug) => slug_projects
                .iter()
                .find(|(known, phid)| known == slug && visible_projects.contains(phid))
                .map(|(_, phid)| phid.as_str().to_owned()),
            Lookup::Nothing => None,
        };
        if let Some(phid) = found {
            objects.push(((*name).to_owned(), phid));
        }
    }
    // Then users, by the names as typed.
    let rest: Vec<String> = stripped
        .iter()
        .map(|(name, _)| name)
        .filter(|name| !objects.iter().any(|(known, _)| known == *name))
        .cloned()
        .collect();
    let users = users_by_lowercase_name(request, &rest).await?;
    for name in &rest {
        if let Some(phid) = users.get(&name.to_ascii_lowercase()) {
            objects.push((name.clone(), phid.clone()));
        }
    }
    Ok(objects)
}

/// `PhabricatorObjectListQuery::execute()` for lists of users and
/// projects: the objects, or the message of the
/// `DifferentialFieldParseException`. `allowed` are the allowed PHID
/// types; with `suffixes`, a trailing `!` marks blocking reviewers.
///
/// Names PHP would resolve to other objects (monograms, commit hashes,
/// PHIDs of other types), and names with unusual characters, are left to
/// PHP.
pub(super) async fn parse_list(
    request: &ApiRequest<'_>,
    value: &str,
    allowed: &[PhidType],
    allow_partial: bool,
    suffixes: bool,
) -> Result<Result<Vec<Listed>, String>, MethodError> {
    let stripped = list_names(value, suffixes)?;
    let objects = load_objects(request, &stripped).await?;

    // The names of objects of types which are not allowed, by type.
    let mut types: Vec<(PhidType, Vec<&str>)> = Vec::new();
    for (name, phid) in &objects {
        let phid_type = phid_type_of(phid);
        match types.iter_mut().find(|(known, _)| *known == phid_type) {
            Some((_, names)) => names.push(name),
            None => types.push((phid_type, vec![name])),
        }
    }
    let invalid: Vec<&str> = types
        .iter()
        .filter(|(phid_type, _)| !allowed.contains(phid_type))
        .flat_map(|(_, names)| names.iter().copied())
        .collect();

    let mut result: Vec<(String, String)> = Vec::new();
    for (name, phid) in &objects {
        if !result.iter().any(|(_, known)| known == phid) {
            result.push((name.clone(), phid.clone()));
        }
    }
    // PHIDs of allowed types go through unchecked.
    let mut missing: Vec<&str> = Vec::new();
    for (name, _) in &stripped {
        if objects.iter().any(|(known, _)| known == name) {
            continue;
        }
        if allowed.contains(&phid_type_of(name)) {
            result.push((name.clone(), name.clone()));
        } else {
            missing.push(name);
        }
    }

    if !allow_partial {
        let message = match (invalid.is_empty(), missing.is_empty()) {
            (false, false) => Some(format!(
                "The objects you have listed include objects of the wrong type ({}) and objects \
                 which do not exist ({}).",
                invalid.join(", "),
                missing.join(", ")
            )),
            (false, true) => Some(format!(
                "The objects you have listed include objects of the wrong type ({}).",
                invalid.join(", ")
            )),
            (true, false) => Some(format!(
                "The objects you have listed include objects which do not exist ({}).",
                missing.join(", ")
            )),
            (true, true) => None,
        };
        if let Some(message) = message {
            return Ok(Err(message));
        }
    }

    Ok(Ok(result
        .into_iter()
        .map(|(name, phid)| Listed {
            blocking: stripped
                .iter()
                .any(|(known, blocking)| *known == name && *blocking),
            phid,
        })
        .collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_names_of_other_objects() {
        for name in [
            "D12",
            "d1",
            "S1",
            "B0012",
            "rMOZ",
            "R2",
            "R2:abc",
            "F3",
            "abcdef1",
            "0123456789",
        ] {
            assert!(may_name_other_object(name), "{name}");
        }
        for name in [
            "alice", "f3", "Dave", "abcdef", "rmoz", "abcdefg", "ABCDEF1",
        ] {
            assert!(!may_name_other_object(name), "{name}");
        }
    }

    #[test]
    fn integer_keys_like_php() {
        assert!(is_integer_key("12"));
        assert!(is_integer_key("-3"));
        assert!(!is_integer_key("1a"));
        assert!(!is_integer_key("-"));
    }
}
