//! What `PhabricatorProjectDatasource` reads of projects: the queries of
//! its prefix and content phases, hashtags, profile images and the icons
//! of `projects.icons`.

use std::collections::HashMap;
use std::fmt::Write as _;

use phab_core::{Config, Phid, hash};
use phab_db::{Database, DbError, rows};
use serde_json::Value;

use crate::search::IconSet;
use crate::storage::{COLUMNS, Project};

/// `PhabricatorProjectQuery` as the datasource runs it: projects whose
/// name starts with `prefix` (`withNamePrefixes()`), or with a datasource
/// token starting with each of `tokens` (`withNameTokens()`), active ones
/// first and then the newest, at most `limit` of them. Not filtered by
/// policy.
pub async fn search(
    database: &Database,
    prefix: Option<&str>,
    tokens: &[String],
    exclude_milestones: bool,
    limit: u32,
) -> Result<Vec<Project>, DbError> {
    let mut joins = String::new();
    let mut binds: Vec<String> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        write!(
            joins,
            " JOIN {table} token_{index} ON token_{index}.projectID = p.id \
             AND token_{index}.token LIKE ?",
            table = database.table("project", "project_datasourcetoken"),
        )
        .expect("writing to a String");
        binds.push(format!("{}%", phab_db::like_escape(token)));
    }
    let mut conditions = Vec::new();
    if let Some(prefix) = prefix {
        conditions.push("p.name LIKE ?");
        binds.push(format!("{}%", phab_db::like_escape(prefix)));
    }
    if exclude_milestones {
        conditions.push("p.milestoneNumber IS NULL");
    }
    let mut sql = format!(
        "SELECT {COLUMNS} FROM {} p{joins}",
        database.table("project", "project"),
    );
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    if !tokens.is_empty() {
        sql.push_str(" GROUP BY p.id");
    }
    sql.push_str(" ORDER BY p.status ASC, p.id DESC LIMIT ?");
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), binds)
        .bind(limit)
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(Project::from_row).collect()
}

/// The hashtags of these projects, in the order they were added
/// (`needSlugs()`).
pub async fn load_slugs(
    database: &Database,
    projects: &[Phid],
) -> Result<HashMap<Phid, Vec<String>>, DbError> {
    let mut slugs: HashMap<Phid, Vec<String>> = HashMap::new();
    if projects.is_empty() {
        return Ok(slugs);
    }
    let sql = format!(
        "SELECT projectPHID, slug FROM {} WHERE projectPHID IN ({}) ORDER BY id",
        database.table("project", "project_slug"),
        phab_db::placeholders(projects.len())
    );
    for row in phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        projects.iter().map(Phid::as_str),
    )
    .fetch_all(database.pool())
    .await?
    {
        slugs
            .entry(Phid::new(rows::text(&row, "projectPHID")?))
            .or_default()
            .push(rows::text(&row, "slug")?);
    }
    Ok(slugs)
}

/// The custom profile images of these projects (`profileImagePHID`).
pub async fn load_profile_images(
    database: &Database,
    projects: &[Phid],
) -> Result<HashMap<Phid, Phid>, DbError> {
    let mut images = HashMap::new();
    if projects.is_empty() {
        return Ok(images);
    }
    let sql = format!(
        "SELECT phid, profileImagePHID FROM {} WHERE phid IN ({})",
        database.table("project", "project"),
        phab_db::placeholders(projects.len())
    );
    for row in phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        projects.iter().map(Phid::as_str),
    )
    .fetch_all(database.pool())
    .await?
    {
        if let Some(image) =
            rows::opt_text(&row, "profileImagePHID")?.filter(|phid| !phid.is_empty())
        {
            images.insert(Phid::new(rows::text(&row, "phid")?), Phid::new(image));
        }
    }
    Ok(images)
}

/// `PhabricatorProjectIconSet` of the configuration.
pub struct Icons(IconSet);

impl Icons {
    /// `projects.icons`, or its default.
    pub fn load(config: &Config) -> Self {
        Self(IconSet::load(config))
    }

    /// `getDisplayIconKey()`: milestones have the milestone icon.
    fn display_key(&self, project: &Project) -> Option<String> {
        if project.is_milestone() {
            return self.0.milestone_key().as_str().map(str::to_owned);
        }
        Some(project.icon.clone())
    }

    fn display_field(&self, project: &Project, field: &str) -> Option<Value> {
        let key = self.display_key(project)?;
        self.0.icon_field(&key, field).cloned()
    }

    /// `getDisplayIconIcon()`, as `idx()` returns it.
    pub fn display_icon(&self, project: &Project) -> Option<Value> {
        self.display_field(project, "icon")
    }

    /// `getDisplayIconName()`, as `idx()` returns it.
    pub fn display_name(&self, project: &Project) -> Option<Value> {
        self.display_field(project, "name")
    }

    /// The builtin image of a project without its own
    /// (`'projects/'.getIconImage($project->getIcon())`), or `None` when
    /// the configured image isn't a string.
    pub fn builtin_image(&self, project: &Project) -> Option<String> {
        match self.0.icon_field(&project.icon, "image") {
            None => Some("projects/v3/briefcase.png".to_owned()),
            Some(Value::Null) => Some("projects/".to_owned()),
            Some(Value::String(image)) => Some(format!("projects/{image}")),
            Some(_) => None,
        }
    }
}

/// `PhabricatorFilesOnDiskBuiltinFile::getBuiltinFileKey()`.
pub fn builtin_file_key(name: &str) -> Result<String, hash::HashError> {
    let digest = hash::digest_to_length(format!("disk(name={name})").as_bytes(), 40)?;
    Ok(format!("builtin:{}", String::from_utf8_lossy(&digest)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected value from PhabricatorFilesOnDiskBuiltinFile.
    #[test]
    fn builtin_keys_match_php() {
        let key = builtin_file_key("projects/v3/briefcase.png").unwrap();
        assert!(
            key.starts_with("builtin:disk(name=projects/v3/brief-"),
            "{key}"
        );
        assert_eq!(key.len(), "builtin:".len() + 40);
    }
}
