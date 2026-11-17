//! `project.search`: `PhabricatorProjectSearchEngine` through
//! `buildConduitResponse()`.
//!
//! Ported: every constraint but the fulltext `query`, with plain PHIDs for
//! members and watchers (usernames and functions go to PHP), the `name`
//! (default), `newest` and `oldest` orders, paging, the fields with the
//! description, the slug map and the `members`, `watchers` and
//! `ancestors` attachments. Left to PHP: `query`, `relevance`, saved
//! queries.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Config, Phid};
use phab_db::{Database, DbError, edges, rows};
use serde_json::{Map, Value, json};

use crate::policy::{self, Graph};
use crate::storage::{self, OBJECT_HAS_WATCHER, Project};

/// `PhabricatorProjectApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorProjectApplication",
    name: "Projects",
};

const QUERY_CLASS: &str = "PhabricatorProjectQuery";

/// `getValidConstraintKeys()` of the engine's fields.
const VALID_CONSTRAINTS: &[&str] = &[
    "name",
    "slugs",
    "members",
    "watchers",
    "isMilestone",
    "isRoot",
    "minDepth",
    "maxDepth",
    "subtypes",
    "icons",
    "colors",
    "parents",
    "ancestors",
    "query",
    "ids",
    "phids",
];

const ATTACHMENTS: &[&str] = &["members", "watchers", "ancestors"];

/// The custom field of `PhabricatorProjectDescriptionField`.
const DESCRIPTION_FIELD: &str = "std:project:internal:description";

/// The builtin orders of `PhabricatorProjectQuery`; `name` comes first, so
/// it is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// By increasing name.
    Name,
    /// By decreasing ID.
    Newest,
    /// By increasing ID.
    Oldest,
}

impl Order {
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let key = match value {
            None | Some(Value::Null) => return Ok(Self::Name),
            Some(Value::String(key)) => key.as_str(),
            Some(_) => return Err(MethodError::Unsupported("order vectors")),
        };
        match key {
            "name" => Ok(Self::Name),
            "newest" | "created" => Ok(Self::Newest),
            "oldest" => Ok(Self::Oldest),
            "relevance" => Err(MethodError::Unsupported("the relevance order")),
            other => Err(pager::core_error(format!(
                "Query \"{QUERY_CLASS}\" does not support a builtin order \"{other}\". \
                 Supported orders are: name, newest, created, oldest, relevance."
            ))),
        }
    }

    /// The column and whether going forward means decreasing values.
    fn column(self) -> (&'static str, bool) {
        match self {
            Self::Name => ("p.name", false),
            Self::Newest => ("p.id", true),
            Self::Oldest => ("p.id", false),
        }
    }
}

/// The ported constraints, as `buildQueryFromParameters()` turns them into
/// query conditions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Filters {
    name_tokens: Vec<String>,
    slugs: Vec<String>,
    members: Option<Vec<String>>,
    watchers: Option<Vec<String>>,
    is_milestone: Option<bool>,
    depth: (Option<i64>, Option<i64>),
    subtypes: Option<Vec<String>>,
    icons: Option<Vec<String>>,
    colors: Option<Vec<String>>,
    parents: Option<Vec<String>>,
    ancestors: Option<Vec<String>>,
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
}

/// A list constraint: an empty list is no constraint.
fn nonempty(list: Vec<String>) -> Option<Vec<String>> {
    Some(list).filter(|list| !list.is_empty())
}

/// `ConduitUserListParameterType` with plain user PHIDs.
fn user_phids(key: &str, value: &Value) -> Result<Option<Vec<String>>, MethodError> {
    let list = search::list(key, value, true)?;
    Ok(nonempty(search::plain_phids(list, "PHID-USER-")?))
}

impl Filters {
    fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        let mut filters = Self::default();
        // String lists and checkboxes take an empty list, which is no
        // constraint; PHID lists and datasources don't.
        let string_list =
            |key: &str, allow_empty: bool| -> Result<Option<Vec<String>>, MethodError> {
                constraints
                    .get(&[key])
                    .map(|(key, value)| search::string_list(key, value, allow_empty))
                    .transpose()
            };
        if let Some((key, value)) = constraints.get(&["name"]) {
            filters.name_tokens = tokenize(&search::string(key, value)?);
        }
        filters.slugs = string_list("slugs", true)?.unwrap_or_default();
        if let Some((key, value)) = constraints.get(&["members"]) {
            filters.members = user_phids(key, value)?;
        }
        if let Some((key, value)) = constraints.get(&["watchers"]) {
            filters.watchers = user_phids(key, value)?;
        }
        if let Some((key, value)) = constraints.get(&["isMilestone"]) {
            filters.is_milestone = Some(search::bool(key, value, strict)?);
        }
        let is_root = constraints
            .get(&["isRoot"])
            .map(|(key, value)| search::bool(key, value, strict))
            .transpose()?;
        let mut min_depth = constraints
            .get(&["minDepth"])
            .map(|(key, value)| search::int(key, value, strict))
            .transpose()?;
        let mut max_depth = constraints
            .get(&["maxDepth"])
            .map(|(key, value)| search::int(key, value, strict))
            .transpose()?;
        filters.subtypes = string_list("subtypes", false)?.and_then(nonempty);
        filters.icons = string_list("icons", true)?.and_then(nonempty);
        filters.colors = string_list("colors", true)?.and_then(nonempty);
        filters.parents = string_list("parents", false)?.and_then(nonempty);
        filters.ancestors = string_list("ancestors", false)?.and_then(nonempty);
        if let Some((key, value)) = constraints.get(&["query"])
            && !search::string(key, value)?.is_empty()
        {
            return Err(MethodError::Unsupported("fulltext project search"));
        }
        if let Some((key, value)) = constraints.get(&["ids"]) {
            filters.ids = Some(search::int_list(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["phids"]) {
            filters.phids = Some(search::string_list(key, value, false)?);
        }

        if let (Some(min), Some(max)) = (min_depth, max_depth)
            && min > max
        {
            return Err(pager::core_error(
                "Search constraint \"minDepth\" must be no larger than search constraint \
                 \"maxDepth\"."
                    .to_owned(),
            ));
        }
        match is_root {
            Some(true) => max_depth = Some(max_depth.map_or(0, |max| max.min(0))),
            Some(false) => min_depth = Some(min_depth.map_or(1, |min| min.max(1))),
            None => {}
        }
        filters.depth = (min_depth, max_depth);
        Ok(filters)
    }

    /// The slugs to look for (`willExecute()`): each as given, and
    /// normalized when it is a valid hashtag.
    fn all_slugs(&self) -> Vec<String> {
        let mut all = Vec::new();
        for slug in &self.slugs {
            if let Some(normal) = valid_slug(slug)
                && !all.contains(&normal)
            {
                all.push(normal);
            }
            if !all.contains(slug) {
                all.push(slug.clone());
            }
        }
        all
    }
}

/// `PhabricatorTypeaheadDatasource::tokenizeString()`, then the five
/// longest tokens (`getNameTokensForQuery()`).
fn tokenize(name: &str) -> Vec<String> {
    let lowered = name.to_ascii_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    for token in lowered
        .trim()
        .split(|character: char| character.is_whitespace() || "[]()-".contains(character))
    {
        if !token.is_empty() && !tokens.iter().any(|known| known == token) {
            tokens.push(token.to_owned());
        }
    }
    // A stable sort, longest first, like PHP 8's arsort().
    tokens.sort_by_key(|token| std::cmp::Reverse(token.len()));
    tokens.truncate(5);
    tokens
}

/// `PhabricatorSlug::normalizeProjectSlug()`, or `None` for slugs which
/// `isValidProjectSlug()` refuses.
fn valid_slug(slug: &str) -> Option<String> {
    const BANNED: &str = "#%&+=? \\<>{}[]'\"`~!@$^*,:;(|)/";
    let mut normal = String::with_capacity(slug.len());
    for character in slug.chars() {
        let character = character.to_ascii_lowercase();
        // Runs of banned characters and of underscores become one "_".
        if character <= '\x19' || character == '_' || BANNED.contains(character) {
            if !normal.ends_with('_') {
                normal.push('_');
            }
        } else {
            normal.push(character);
        }
    }
    let normal = if normal == "_" {
        normal
    } else {
        normal.trim_matches('_').to_owned()
    };
    let normal = match normal.as_str() {
        "." => "dot".to_owned(),
        ".." => "dotdot".to_owned(),
        _ => normal,
    };
    (normal != "_").then_some(normal)
}

/// `PhabricatorProjectQuery` with the ported constraints.
struct Projects<'a> {
    request: &'a ApiRequest<'a>,
    filters: Filters,
    order: Order,
    /// `projectPath` and `projectDepth` of the `ancestors` constraint.
    ancestor_paths: Vec<(String, i64)>,
}

impl Projects<'_> {
    async fn name_of(&self, id: u64) -> Result<String, MethodError> {
        let database = &self.request.context.database;
        let sql = format!(
            "SELECT name FROM {} WHERE id = ?",
            database.table("project", "project")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
            .ok_or_else(|| MethodError::internal("the cursor's project vanished"))?;
        Ok(rows::text(&row, "name")?)
    }

    /// `buildJoinClauseParts()`: members, watchers, slugs and name tokens.
    fn add_joins(&self, clauses: &mut Clauses) {
        let database = &self.request.context.database;
        let filters = &self.filters;
        let edge_table = database.table("project", "edge");
        if let Some(members) = &filters.members {
            clauses.join(
                format!("JOIN {edge_table} e ON e.src = p.phid AND e.type = ?"),
                [json!(edges::PROJECT_MATERIALIZED_MEMBER)],
            );
            clauses.in_list("e.dst", search::strings(members));
        }
        if let Some(watchers) = &filters.watchers {
            clauses.join(
                format!("JOIN {edge_table} w ON w.src = p.phid AND w.type = ?"),
                [json!(OBJECT_HAS_WATCHER)],
            );
            clauses.in_list("w.dst", search::strings(watchers));
        }
        if !filters.slugs.is_empty() {
            clauses.join(
                format!(
                    "JOIN {} slug ON slug.projectPHID = p.phid",
                    database.table("project", "project_slug")
                ),
                [],
            );
            clauses.in_list("slug.slug", search::strings(&filters.all_slugs()));
        }
        for (index, token) in filters.name_tokens.iter().enumerate() {
            clauses.join(
                format!(
                    "JOIN {} token_{index} ON token_{index}.projectID = p.id \
                     AND token_{index}.token LIKE ?",
                    database.table("project", "project_datasourcetoken")
                ),
                [json!(format!("{}%", phab_db::like_escape(token)))],
            );
        }
    }

    /// `buildWhereClauseParts()` for the other constraints.
    fn add_conditions(&self, clauses: &mut Clauses) {
        let filters = &self.filters;
        if let Some(ids) = &filters.ids {
            clauses.in_list("p.id", ids.iter().map(|id| json!(id)));
        }
        for (column, values) in [
            ("p.phid", &filters.phids),
            ("p.icon", &filters.icons),
            ("p.color", &filters.colors),
            ("p.parentProjectPHID", &filters.parents),
        ] {
            if let Some(values) = values {
                clauses.in_list(column, search::strings(values));
            }
        }
        if filters.ancestors.is_some() {
            let parts: Vec<&str> = self
                .ancestor_paths
                .iter()
                .map(|_| "(p.projectPath LIKE ? AND p.projectDepth > ?)")
                .collect();
            let binds = self.ancestor_paths.iter().flat_map(|(path, depth)| {
                [
                    json!(format!("{}%", phab_db::like_escape(path))),
                    json!(depth),
                ]
            });
            clauses.condition(parts.join(" OR "), binds);
            clauses.condition("p.parentProjectPHID IS NOT NULL", []);
        }
        match filters.is_milestone {
            Some(true) => clauses.condition("p.milestoneNumber IS NOT NULL", []),
            Some(false) => clauses.condition("p.milestoneNumber IS NULL", []),
            None => {}
        }
        if let Some(min) = filters.depth.0 {
            clauses.condition("p.projectDepth >= ?", [json!(min)]);
        }
        if let Some(max) = filters.depth.1 {
            clauses.condition("p.projectDepth <= ?", [json!(max)]);
        }
        if let Some(subtypes) = &filters.subtypes {
            clauses.in_list("p.subtype", search::strings(subtypes));
        }
    }
}

impl PolicyAwareQuery for Projects<'_> {
    type Row = Project;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &Project) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<Project>, MethodError> {
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("p.id", ids.iter().map(|id| json!(id)));
        } else {
            self.add_joins(&mut clauses);
            self.add_conditions(&mut clauses);
        }
        let (column, descending) = self.order.column();
        let descending = descending == (direction == Direction::Forward);
        let (comparison, order) = if descending {
            ("<", "DESC")
        } else {
            (">", "ASC")
        };
        if let Some(after) = after {
            let value = if self.order == Order::Name {
                json!(self.name_of(after).await?)
            } else {
                json!(after)
            };
            clauses.condition(format!("{column} {comparison} ?"), [value]);
        }
        let mut tail = String::new();
        if clauses.has_joins() {
            tail.push_str(" GROUP BY p.id");
        }
        let _ = write!(tail, " ORDER BY {column} {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let database = &self.request.context.database;
        let sql = clauses.sql(
            &format!(
                "SELECT p.phid FROM {} p",
                database.table("project", "project")
            ),
            &tail,
        );
        let page: Vec<Phid> = clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| rows::text(row, "phid").map(Phid::new))
            .collect::<Result<_, _>>()?;
        let mut loaded: HashMap<Phid, Project> = storage::load_by_phid(database, &page)
            .await?
            .into_iter()
            .map(|project| (project.phid.clone(), project))
            .collect();
        Ok(page.iter().filter_map(|phid| loaded.remove(phid)).collect())
    }

    async fn filter(&self, rows: Vec<Project>) -> Result<Vec<Project>, MethodError> {
        let context = self.request.context;
        let (visible, _) = policy::visible_projects(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        Ok(visible)
    }
}

/// `projects.icons` and `projects.colors`, or their defaults.
struct IconSet {
    icons: Vec<Value>,
    colors: Vec<Value>,
}

impl IconSet {
    fn load(config: &Config) -> Self {
        static DEFAULTS: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
            serde_json::from_str(include_str!("../data/icon-defaults.json"))
                .expect("valid icon defaults")
        });
        let list = |key: &str, default: &str| {
            config
                .value(key)
                .and_then(Value::as_array)
                .or_else(|| DEFAULTS[default].as_array())
                .cloned()
                .unwrap_or_default()
        };
        Self {
            icons: list("projects.icons", "icons"),
            colors: list("projects.colors", "colors"),
        }
    }

    /// `getMilestoneIconKey()`.
    fn milestone_key(&self) -> Value {
        self.icons
            .iter()
            .find(|icon| icon["special"] == "milestone")
            .map_or(Value::Null, |icon| icon["key"].clone())
    }

    /// `getIconSpec()`'s `name` and `icon`.
    fn icon(&self, key: &Value) -> (Value, Value) {
        self.icons
            .iter()
            .find(|icon| icon["key"] == *key)
            .map_or((Value::Null, Value::Null), |icon| {
                (icon["name"].clone(), icon["icon"].clone())
            })
    }

    /// `getColorName()`: the last specification with the key wins, like
    /// `ipull()`.
    fn color_name(&self, key: &str) -> Value {
        self.colors
            .iter()
            .rev()
            .find(|color| color["key"] == key)
            .map_or(Value::Null, |color| color["name"].clone())
    }
}

/// What a page needs beyond its projects.
struct Extra {
    path_depth: HashMap<Phid, (i64, i64, i64, String)>,
    descriptions: HashMap<Phid, String>,
    members: HashMap<Phid, Vec<Phid>>,
    watchers: HashMap<Phid, Vec<Phid>>,
}

/// `projectDepth`, `dateCreated`, `dateModified` and `subtype` of
/// projects.
async fn load_columns(
    database: &Database,
    phids: &[Phid],
) -> Result<HashMap<Phid, (i64, i64, i64, String)>, DbError> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT phid, projectDepth, dateCreated, dateModified, subtype FROM {} WHERE phid IN ({})",
        database.table("project", "project"),
        phab_db::placeholders(phids.len())
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "phid")?),
                (
                    rows::int(row, "projectDepth")?,
                    rows::int(row, "dateCreated")?,
                    rows::int(row, "dateModified")?,
                    rows::text(row, "subtype")?,
                ),
            ))
        })
        .collect()
}

/// The descriptions of projects (`PhabricatorProjectDescriptionField`).
async fn load_descriptions(
    database: &Database,
    phids: &[Phid],
) -> Result<HashMap<Phid, String>, DbError> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT objectPHID, fieldValue FROM {} WHERE fieldIndex = ? AND objectPHID IN ({})",
        database.table("project", "project_customfieldstorage"),
        phab_db::placeholders(phids.len())
    );
    let index = phab_core::hash::digest_for_index(DESCRIPTION_FIELD.as_bytes());
    phab_db::bind_all(
        phab_db::sqlx::query(&sql).bind(index),
        phids.iter().map(Phid::as_str),
    )
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(|row| {
        Ok((
            Phid::new(rows::text(row, "objectPHID")?),
            rows::text(row, "fieldValue")?,
        ))
    })
    .collect()
}

/// The destinations of `sources`' edges of one type, by source.
async fn load_edge_map(
    database: &Database,
    sources: &[Phid],
    edge_type: i64,
) -> Result<HashMap<Phid, Vec<Phid>>, DbError> {
    let mut map: HashMap<Phid, Vec<Phid>> = HashMap::new();
    for edge in edges::load_edges(database, sources, &[edge_type], None).await? {
        map.entry(edge.src).or_default().push(edge.dst);
    }
    Ok(map)
}

/// `loadSlugs()`: which project each queried slug found, as given or
/// normalized.
async fn slug_map(
    database: &Database,
    filters: &Filters,
    projects: &[Project],
) -> Result<Map<String, Value>, DbError> {
    let mut map = Map::new();
    let mut unknown: Vec<(&String, String)> = Vec::new();
    for input in &filters.slugs {
        let Some(normal) = valid_slug(input) else {
            continue;
        };
        if unknown.iter().any(|(known, _)| *known == input) {
            continue;
        }
        let primary = |slug: &str| {
            projects
                .iter()
                .rev()
                .find(|project| project.primary_slug.as_deref() == Some(slug))
        };
        let found = [input.as_str(), normal.as_str()]
            .into_iter()
            .find_map(|slug| primary(slug).map(|project| (slug.to_owned(), project.phid.clone())));
        match found {
            Some((slug, phid)) => {
                map.insert(
                    input.clone(),
                    json!({"slug": slug, "projectPHID": phid.as_str()}),
                );
            }
            None => unknown.push((input, normal)),
        }
    }
    if unknown.is_empty() || projects.is_empty() {
        return Ok(map);
    }
    let normals: Vec<&str> = unknown.iter().map(|(_, normal)| normal.as_str()).collect();
    let sql = format!(
        "SELECT slug, projectPHID FROM {} WHERE projectPHID IN ({}) AND slug IN ({})",
        database.table("project", "project_slug"),
        phab_db::placeholders(projects.len()),
        phab_db::placeholders(normals.len()),
    );
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        projects.iter().map(|project| project.phid.as_str()),
    );
    let extra: HashMap<String, String> = phab_db::bind_all(query, normals)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| Ok((rows::text(row, "slug")?, rows::text(row, "projectPHID")?)))
        .collect::<Result<_, DbError>>()?;
    for (input, normal) in unknown {
        let found = [input.as_str(), normal.as_str()]
            .into_iter()
            .find_map(|slug| extra.get(slug).map(|phid| (slug.to_owned(), phid.clone())));
        if let Some((slug, phid)) = found {
            map.insert(input.clone(), json!({"slug": slug, "projectPHID": phid}));
        }
    }
    Ok(map)
}

fn reference(project: &Project) -> Value {
    json!({"id": project.id, "phid": project.phid.as_str(), "name": project.name})
}

/// One result: `getFieldValuesForConduit()` and the extensions' fields
/// (spaces, dates, policies, custom fields), and the attachments asked for.
fn result(
    project: &Project,
    graph: &Graph,
    icons: &IconSet,
    extra: &Extra,
    requested: &[&'static str],
) -> Result<Value, MethodError> {
    let (depth, created, modified, subtype) = extra
        .path_depth
        .get(&project.phid)
        .cloned()
        .ok_or_else(|| MethodError::internal("project row vanished"))?;
    let icon_key = if project.is_milestone() {
        icons.milestone_key()
    } else {
        json!(project.icon)
    };
    let (icon_name, icon_icon) = icons.icon(&icon_key);
    let parent = graph.parent(project);
    // Milestones have their parent's policies.
    let policies = match parent {
        Some(parent) if project.is_milestone() => parent,
        _ => project,
    };
    let mut fields = json!({
        "name": project.name,
        "slug": project.primary_slug,
        "subtype": subtype,
        "milestone": project.milestone_number,
        "depth": depth,
        "parent": parent.map(reference),
        "icon": {"key": icon_key, "name": icon_name, "icon": icon_icon},
        "color": {"key": project.color, "name": icons.color_name(&project.color)},
        "spacePHID": project.space_phid.as_ref().map(Phid::as_str),
        "dateCreated": created,
        "dateModified": modified,
        "policy": {
            "view": policies.view_policy,
            "edit": policies.edit_policy,
            "join": policies.join_policy,
        },
    });
    fields["description"] = json!(extra.descriptions.get(&project.phid));
    let mut attachments = Map::new();
    for key in requested {
        let value = match *key {
            "members" => {
                // Milestones have their parent's members.
                let source = if project.is_milestone() {
                    project.parent_phid.as_ref().unwrap_or(&project.phid)
                } else {
                    &project.phid
                };
                let members: Vec<Value> = extra
                    .members
                    .get(source)
                    .into_iter()
                    .flatten()
                    .map(|phid| json!({"phid": phid.as_str()}))
                    .collect();
                json!({"members": members})
            }
            "watchers" => {
                let watchers: Vec<Value> = extra
                    .watchers
                    .get(&project.phid)
                    .into_iter()
                    .flatten()
                    .map(|phid| json!({"phid": phid.as_str()}))
                    .collect();
                json!({"watchers": watchers})
            }
            _ => {
                // Ancestors by increasing depth.
                let mut ancestors = Vec::new();
                let mut cursor = graph.parent(project);
                let mut seen = HashSet::new();
                while let Some(ancestor) = cursor {
                    if !seen.insert(ancestor.phid.clone()) {
                        break;
                    }
                    ancestors.push(reference(ancestor));
                    cursor = graph.parent(ancestor);
                }
                ancestors.reverse();
                json!({"ancestors": ancestors})
            }
        };
        attachments.insert((*key).to_owned(), value);
    }
    Ok(json!({
        "id": project.id,
        "type": "PROJ",
        "phid": project.phid.as_str(),
        "fields": fields,
        "attachments": attachments,
    }))
}

/// `projectPath` and `projectDepth` of the projects of the `ancestors`
/// constraint; none means the query finds nothing.
async fn ancestor_paths(
    database: &Database,
    phids: &[String],
) -> Result<Vec<(String, i64)>, DbError> {
    let sql = format!(
        "SELECT projectPath, projectDepth FROM {} WHERE phid IN ({})",
        database.table("project", "project"),
        phab_db::placeholders(phids.len())
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(String::as_str))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                rows::text(row, "projectPath")?,
                rows::int(row, "projectDepth")?,
            ))
        })
        .collect()
}

/// `project.search`.
pub struct ProjectSearch;

impl Method for ProjectSearch {
    fn name(&self) -> &'static str {
        "project.search"
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
            let requested =
                search::requested_attachments(request.value("attachments"), ATTACHMENTS)?;
            let order = Order::parse(request.value("order"))?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let database = &request.context.database;
            let ancestor_paths = match &filters.ancestors {
                Some(ancestors) => ancestor_paths(database, ancestors).await?,
                None => Vec::new(),
            };
            let (start, direction) = paging.start();
            let empty = filters.ancestors.is_some() && ancestor_paths.is_empty();
            let query = Projects {
                request,
                filters,
                order,
                ancestor_paths,
            };
            // PhabricatorEmptyQueryException: no results, cursors still
            // checked.
            let found = if empty {
                Vec::new()
            } else {
                pager::execute(&query, start, direction, paging.size.value + 1).await?
            };
            let slugs = slug_map(database, &query.filters, &found).await?;
            let page = paging.pager().slice(found);
            let graph = Graph::load(database, &page.items).await?;
            let phids: Vec<Phid> = page
                .items
                .iter()
                .map(|project| project.phid.clone())
                .collect();
            let member_sources: Vec<Phid> = page
                .items
                .iter()
                .map(|project| {
                    if project.is_milestone() {
                        project
                            .parent_phid
                            .clone()
                            .unwrap_or_else(|| project.phid.clone())
                    } else {
                        project.phid.clone()
                    }
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let extra = Extra {
                path_depth: load_columns(database, &phids).await?,
                descriptions: load_descriptions(database, &phids).await?,
                members: if requested.contains(&"members") {
                    load_edge_map(
                        database,
                        &member_sources,
                        edges::PROJECT_MATERIALIZED_MEMBER,
                    )
                    .await?
                } else {
                    HashMap::new()
                },
                watchers: if requested.contains(&"watchers") {
                    load_edge_map(database, &phids, OBJECT_HAS_WATCHER).await?
                } else {
                    HashMap::new()
                },
            };
            let icons = IconSet::load(&request.context.config);
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|project| result(project, &graph, &icons, &extra, &requested))
                .collect::<Result<_, _>>()?;
            let mut response = search::response(request, &data, &paging, &page, |project| {
                project.id.to_string()
            });
            response["maps"] = json!({"slugMap": slugs});
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: &Value) -> Result<Filters, MethodError> {
        Filters::parse(&Constraints::parse(Some(value), VALID_CONSTRAINTS)?, true)
    }

    #[test]
    fn slugs_normalize_like_php() {
        assert_eq!(
            valid_slug("Firefox Build"),
            Some("firefox_build".to_owned())
        );
        assert_eq!(valid_slug("a?"), Some("a".to_owned()));
        assert_eq!(valid_slug("??"), None);
        assert_eq!(valid_slug(".."), Some("dotdot".to_owned()));
        assert_eq!(valid_slug("a/b"), Some("a_b".to_owned()));
        let filters = Filters {
            slugs: vec!["Firefox".to_owned(), "firefox".to_owned(), "??".to_owned()],
            ..Filters::default()
        };
        assert_eq!(filters.all_slugs(), ["firefox", "Firefox", "??"]);
    }

    #[test]
    fn names_tokenize_like_php() {
        assert_eq!(
            tokenize("  Firefox (Build)-Tools x "),
            ["firefox", "build", "tools", "x"]
        );
        assert_eq!(
            tokenize("a bb ccc dddd eeeee ffffff"),
            ["ffffff", "eeeee", "dddd", "ccc", "bb"]
        );
        assert_eq!(tokenize("  "), Vec::<String>::new());
    }

    #[test]
    fn depth_constraints_follow_php() {
        assert_eq!(
            parse(&json!({"isRoot": true})).unwrap().depth,
            (None, Some(0))
        );
        assert_eq!(
            parse(&json!({"isRoot": false, "minDepth": 0, "maxDepth": 3}))
                .unwrap()
                .depth,
            (Some(1), Some(3))
        );
        assert_eq!(
            parse(&json!({"minDepth": 3, "maxDepth": 1})),
            Err(pager::core_error(
                "Search constraint \"minDepth\" must be no larger than search constraint \
                 \"maxDepth\"."
                    .to_owned()
            ))
        );
        assert!(matches!(
            parse(&json!({"members": ["alice"]})),
            Err(MethodError::Unsupported(_))
        ));
        assert_eq!(parse(&json!({"members": []})).unwrap().members, None);
    }

    #[test]
    fn orders() {
        assert_eq!(Order::parse(None), Ok(Order::Name));
        assert_eq!(Order::parse(Some(&json!("created"))), Ok(Order::Newest));
        assert!(Order::parse(Some(&json!("nope"))).is_err());
    }
}
