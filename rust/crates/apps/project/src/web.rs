//! The edits of the web interface (`PhabricatorProjectEditEngine`'s form,
//! `PhabricatorProjectUpdateController` and
//! `PhabricatorProjectWatchController`), through [`crate::editor`] with a
//! web content source, and what their forms show.

use phab_conduit::pager::{self, Direction};
use phab_conduit::search::Constraints;
use phab_conduit::{ApiRequest, MethodError};
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value, json};

use crate::editor::{self, Target};
pub use crate::editor::{Change, Operation, Source};
use crate::search::{Filters, IconSet, Order, Projects};
use crate::{Project, edit};

/// A builtin query of `PhabricatorProjectSearchEngine`: every one hides
/// milestones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// `memberPHIDs` (Joined).
    pub member: Option<String>,
    /// `watcherPHIDs` (Watching).
    pub watcher: Option<String>,
    /// `status` `active`; `all` shows archived projects too.
    pub active_only: bool,
}

/// A page of `query` as the list shows it, by name: up to `limit`
/// projects the viewer sees after the project `after`, and whether there
/// are more.
pub async fn list(
    request: &ApiRequest<'_>,
    query: &ListQuery,
    after: Option<i64>,
    limit: usize,
) -> Result<(Vec<Project>, bool), MethodError> {
    let filters = Filters {
        members: query.member.clone().map(|phid| vec![phid]),
        watchers: query.watcher.clone().map(|phid| vec![phid]),
        is_milestone: Some(false),
        statuses: query
            .active_only
            .then(|| vec![crate::storage::STATUS_ACTIVE.to_owned()]),
        ..Filters::default()
    };
    let projects = Projects {
        request,
        filters,
        order: Order::Name,
        ancestor_paths: Vec::new(),
    };
    let cursor = after.map(|id| pager::Cursor {
        text: id.to_string(),
        truthy: id != 0,
    });
    let mut found =
        pager::execute(&projects, cursor.as_ref(), Direction::Forward, limit + 1).await?;
    let more = found.len() > limit;
    found.truncate(limit);
    Ok((found, more))
}

/// A saved query of `PhabricatorProjectSearchEngine`, as its fields read
/// it (`buildQueryFromParameters()`): empty values are no constraint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedQuery {
    /// `name`: name tokens.
    pub name: String,
    /// `slugs`.
    pub slugs: Vec<String>,
    /// `memberPHIDs`: plain user PHIDs (functions are left to PHP).
    pub members: Vec<String>,
    /// `watcherPHIDs`.
    pub watchers: Vec<String>,
    /// `status`: `active`, `archived`, or anything else for all.
    pub status: String,
    /// `isMilestone`.
    pub is_milestone: Option<bool>,
    /// `isRoot`.
    pub is_root: Option<bool>,
    /// `minDepth` and `maxDepth`.
    pub depth: (Option<i64>, Option<i64>),
    /// `icons`.
    pub icons: Vec<String>,
    /// `colors`.
    pub colors: Vec<String>,
    /// `parentPHIDs`.
    pub parents: Vec<String>,
    /// `order`: a builtin order key, or the default (`name`).
    pub order: Option<String>,
}

/// A page of the saved `query` by its order: up to `limit` projects the
/// viewer sees after the project `after`, and whether there are more.
/// What `project.search` leaves to PHP is [`MethodError::Unsupported`]
/// here too, like user functions and the relevance order.
pub async fn list_saved(
    request: &ApiRequest<'_>,
    query: &SavedQuery,
    after: Option<i64>,
    limit: usize,
) -> Result<(Vec<Project>, bool), MethodError> {
    let mut constraints = Map::new();
    let mut add = |key: &str, value: Value| {
        constraints.insert(key.to_owned(), value);
    };
    if !query.name.is_empty() {
        add("name", json!(query.name));
    }
    for (key, list) in [
        ("slugs", &query.slugs),
        ("members", &query.members),
        ("watchers", &query.watchers),
        ("icons", &query.icons),
        ("colors", &query.colors),
        ("parents", &query.parents),
    ] {
        if !list.is_empty() {
            add(key, json!(list));
        }
    }
    if let Some(is_milestone) = query.is_milestone {
        add("isMilestone", json!(is_milestone));
    }
    if let Some(is_root) = query.is_root {
        add("isRoot", json!(is_root));
    }
    if let Some(min) = query.depth.0 {
        add("minDepth", json!(min));
    }
    if let Some(max) = query.depth.1 {
        add("maxDepth", json!(max));
    }
    let constraints = Value::Object(constraints);
    let mut filters = Filters::parse(
        &Constraints::parse(Some(&constraints), crate::search::VALID_CONSTRAINTS)?,
        true,
    )?;
    filters.statuses = match query.status.as_str() {
        "active" => Some(vec![crate::storage::STATUS_ACTIVE.to_owned()]),
        "archived" => Some(vec![crate::storage::STATUS_ARCHIVED.to_owned()]),
        _ => None,
    };
    let order = Order::parse(query.order.as_ref().map(|order| json!(order)).as_ref())?;
    let projects = Projects {
        request,
        filters,
        order,
        ancestor_paths: Vec::new(),
    };
    let cursor = after.map(|id| pager::Cursor {
        text: id.to_string(),
        truthy: id != 0,
    });
    let mut found =
        pager::execute(&projects, cursor.as_ref(), Direction::Forward, limit + 1).await?;
    let more = found.len() > limit;
    found.truncate(limit);
    Ok((found, more))
}

/// An icon of `projects.icons`, as the form offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconOption {
    /// The key the form posts.
    pub key: String,
    /// The Font Awesome class.
    pub icon: String,
    /// The label.
    pub name: String,
}

/// The icons and colors the form offers (`PhabricatorIconSetEditField`
/// and the color map), in configuration order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Choices {
    /// The icons which are not disabled.
    pub icons: Vec<IconOption>,
    /// `getColorMap()`: the colors' keys and names.
    pub colors: Vec<(String, String)>,
}

impl Choices {
    /// The choices of `config`.
    pub fn load(config: &phab_core::Config) -> Self {
        let set = IconSet::load(config);
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let icons = set
            .icons()
            .iter()
            .filter(|icon| !phab_core::php::is_truthy(&icon["disabled"]))
            .filter(|icon| icon["key"].is_string())
            .map(|icon| IconOption {
                key: text(&icon["key"]),
                icon: text(&icon["icon"]),
                name: text(&icon["name"]),
            })
            .collect();
        let mut colors: Vec<(String, String)> = Vec::new();
        for color in set.colors() {
            let Some(key) = color["key"].as_str() else {
                continue;
            };
            // `ipull()`: the last one with a key wins, in the first place.
            let name = text(&color["name"]);
            match colors.iter_mut().find(|(known, _)| known == key) {
                Some(known) => known.1 = name,
                None => colors.push((key.to_owned(), name)),
            }
        }
        Self { icons, colors }
    }

    /// Whether the form may post `icon`.
    pub fn has_icon(&self, icon: &str) -> bool {
        self.icons.iter().any(|option| option.key == icon)
    }

    /// Whether the form may post `color`.
    pub fn has_color(&self, color: &str) -> bool {
        self.colors.iter().any(|(key, _)| key == color)
    }
}

/// `PhabricatorIconSet::getIconLabel()`: the name of the icon `key` of
/// `projects.icons`, disabled or not, or the key itself.
pub fn icon_label(config: &phab_core::Config, key: &str) -> String {
    IconSet::load(config)
        .icons()
        .iter()
        .find(|icon| icon["key"].as_str() == Some(key))
        .and_then(|icon| icon["name"].as_str())
        .unwrap_or(key)
        .to_owned()
}

/// Every hashtag of the project, in ID order (`needSlugs()`).
pub async fn slugs(database: &Database, project: &Project) -> Result<Vec<String>, DbError> {
    let sql = format!(
        "SELECT slug FROM {} WHERE projectPHID = ? ORDER BY id",
        database.table("project", "project_slug")
    );
    phab_db::sqlx::query(&sql)
        .bind(project.phid.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| rows::text(row, "slug"))
        .collect()
}

/// The additional hashtags the form shows: the project's hashtags in ID
/// order, without the primary one (`buildCustomEditFields()`).
pub async fn additional_slugs(
    database: &Database,
    project: &Project,
) -> Result<Vec<String>, DbError> {
    let sql = format!(
        "SELECT slug FROM {} WHERE projectPHID = ? ORDER BY id",
        database.table("project", "project_slug")
    );
    let mut slugs: Vec<String> = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .bind(project.phid.as_str())
        .fetch_all(database.pool())
        .await?
    {
        let slug = rows::text(&row, "slug")?;
        if Some(&slug) != project.primary_slug.as_ref() && !slugs.contains(&slug) {
            slugs.push(slug);
        }
    }
    Ok(slugs)
}

/// Whether `slugs` are hashtags as the editor stores them: a
/// `project:slugs` transaction which posts them again has no effect.
/// Only printable ASCII is normalized as PHP does.
pub fn slugs_are_normal(slugs: &[String]) -> bool {
    slugs
        .iter()
        .all(|slug| slug.bytes().all(|byte| (b' '..=b'~').contains(&byte)))
        && editor::normalize_slugs(slugs) == slugs
}

/// `getIsMembershipLocked()` and `getHasSubprojects()`.
pub async fn membership_flags(
    database: &Database,
    project: &Project,
) -> Result<(bool, bool), DbError> {
    let sql = format!(
        "SELECT isMembershipLocked, hasSubprojects FROM {} WHERE id = ?",
        database.table("project", "project")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(project.id)
        .fetch_one(database.pool())
        .await?;
    Ok((
        rows::flag(&row, "isMembershipLocked")?,
        rows::flag(&row, "hasSubprojects")?,
    ))
}

/// What, installed, adds fields to PHP's form or changes how it writes,
/// which the Rust form doesn't show: custom fields, subtypes, spaces and
/// forms configured in the web UI.
pub async fn unported_form(request: &ApiRequest<'_>) -> Result<Option<&'static str>, MethodError> {
    let config = &request.context.config;
    if config.value("projects.fields").is_some()
        || config
            .value("projects.custom-field-definitions")
            .is_some_and(phab_core::php::is_truthy)
    {
        return Ok(Some("project custom fields"));
    }
    if config.value("projects.subtypes").is_some() {
        return Ok(Some("project subtypes"));
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} LIMIT 1",
        database.table("spaces", "spaces_namespace")
    );
    if phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Ok(Some("projects in spaces"));
    }
    if has_configured_forms(request).await? {
        return Ok(Some("configured project forms"));
    }
    Ok(None)
}

/// What, installed, changes what PHP's project searches find or which
/// fields they have, which the Rust list doesn't apply: custom fields and
/// spaces.
pub async fn unported_search(
    request: &ApiRequest<'_>,
) -> Result<Option<&'static str>, MethodError> {
    let config = &request.context.config;
    if config.value("projects.fields").is_some()
        || config
            .value("projects.custom-field-definitions")
            .is_some_and(phab_core::php::is_truthy)
    {
        return Ok(Some("project custom fields"));
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} LIMIT 1",
        database.table("spaces", "spaces_namespace")
    );
    if phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Ok(Some("projects in spaces"));
    }
    Ok(None)
}

/// Whether forms configured in the web UI change what PHP's form shows
/// or writes; the Rust form doesn't apply them.
pub async fn has_configured_forms(request: &ApiRequest<'_>) -> Result<bool, MethodError> {
    match edit::check_configuration(request).await {
        Ok(()) => Ok(false),
        Err(MethodError::Unsupported(_)) => Ok(true),
        Err(other) => Err(other),
    }
}

/// Applies `changes` to `project` as the web interface does, with a web
/// content source. What is not ported is [`MethodError::Unsupported`],
/// before anything is written; the editor's validation errors, which PHP
/// shows on the form, are [`MethodError::Conduit`]. Returns the
/// transaction PHIDs.
pub async fn apply(
    request: &ApiRequest<'_>,
    project: Project,
    changes: Vec<Change>,
) -> Result<Vec<String>, MethodError> {
    let target = Target {
        project: Some(project),
    };
    let outcome = editor::apply(request, target, changes, Source::Web).await?;
    Ok(outcome.transactions)
}
