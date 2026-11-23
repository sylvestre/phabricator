//! `PhabricatorHandleQuery` as a mail recipient: the handles of the users,
//! projects, repositories and applications a mail names.

use std::collections::HashMap;

use phab_app_project as project;
use phab_core::{Config, Phid, PhidType};
use phab_db::{Database, DbError, rows};
use phab_policy::Viewer;

use crate::Error;

/// The Herald application, which authors the transactions of its rules.
pub(crate) const HERALD_APPLICATION: &str = "PHID-APPS-PhabricatorHeraldApplication";

/// What mail shows of an object (`PhabricatorObjectHandle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handle {
    /// `getName()`.
    pub(crate) name: String,
    /// `getLinkName()`: the name of users, the full name of the others.
    pub(crate) link_name: String,
    /// `getCommandLineObjectName()`.
    pub(crate) command_line_name: String,
    /// `getMailStampName()`.
    pub(crate) mail_stamp_name: Option<String>,
    /// `getObjectName()`.
    pub(crate) object_name: String,
    /// `isComplete()`: loaded, and visible to the viewer.
    pub(crate) complete: bool,
}

impl Handle {
    fn named(name: String) -> Self {
        Self {
            link_name: name.clone(),
            command_line_name: name.clone(),
            object_name: name.clone(),
            name,
            mail_stamp_name: None,
            complete: false,
        }
    }

    fn restricted(type_name: &str) -> Self {
        Self::named(format!("Restricted {type_name}"))
    }

    fn unknown(type_name: &str) -> Self {
        Self::named(format!("Unknown Object ({type_name})"))
    }
}

/// The handles of a viewer: [`Handles::get`] is `None` for the PHIDs of
/// types not loaded here.
#[derive(Debug, Default)]
pub(crate) struct Handles {
    handles: HashMap<String, Handle>,
}

impl Handles {
    /// The handle of `phid`, if its type is ported.
    pub(crate) fn get(&self, phid: &str) -> Option<&Handle> {
        self.handles.get(phid)
    }

    /// The handle of `phid`, which a title or a list must name.
    pub(crate) fn require(&self, phid: &str) -> Result<&Handle, Error> {
        self.get(phid)
            .ok_or_else(|| Error::unsupported(format!("the handle of {phid}")))
    }
}

/// A repository, as its handle shows it.
struct Repository {
    phid: String,
    monogram: String,
    name: String,
}

async fn load_repositories(database: &Database, phids: &[&str]) -> Result<Vec<Repository>, Error> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, phid, name, callsign FROM {} WHERE phid IN ({})",
        database.table("repository", "repository"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().copied())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let mut repositories = Vec::new();
    for row in found {
        let callsign = rows::opt_text(&row, "callsign")?.filter(|callsign| !callsign.is_empty());
        let monogram = match callsign {
            Some(callsign) => format!("r{callsign}"),
            None => format!("R{}", rows::int(&row, "id")?),
        };
        repositories.push(Repository {
            phid: rows::text(&row, "phid")?,
            monogram,
            name: rows::text(&row, "name")?,
        });
    }
    Ok(repositories)
}

/// The handles of projects, as `viewer` sees them.
async fn load_projects(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    projects: &[Phid],
    now: i64,
    handles: &mut Handles,
) -> Result<(), Error> {
    let loaded = project::storage::load_by_phid(database, projects).await?;
    let loaded_phids: Vec<Phid> = loaded.iter().map(|project| project.phid.clone()).collect();
    let (visible, graph) = project::policy::visible_projects(database, config, viewer, loaded, now)
        .await
        .map_err(|error| Error::unsupported(format!("a project policy check: {error}")))?;
    for project in &visible {
        let name = project.display_name(graph.parent(project));
        let slug = project
            .primary_slug
            .as_ref()
            .filter(|slug| !slug.is_empty());
        handles.handles.insert(
            project.phid.as_str().to_owned(),
            Handle {
                link_name: name.clone(),
                command_line_name: slug.map_or_else(
                    || project.phid.as_str().to_owned(),
                    |slug| format!("#{slug}"),
                ),
                mail_stamp_name: slug.map(|slug| format!("_#{slug}")),
                object_name: slug.map_or_else(|| name.clone(), |slug| format!("#{slug}")),
                name,
                complete: true,
            },
        );
    }
    for phid in &loaded_phids {
        handles
            .handles
            .entry(phid.as_str().to_owned())
            .or_insert_with(|| Handle::restricted("Project"));
    }
    for phid in projects {
        handles
            .handles
            .entry(phid.as_str().to_owned())
            .or_insert_with(|| Handle::unknown("Project"));
    }
    Ok(())
}

/// Loads the handles of `phids` as `viewer` sees them. Repositories are
/// loaded as visible: the viewers here can see the revision, whose view
/// policy includes its repository's.
pub(crate) async fn load(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    phids: &[String],
    now: i64,
) -> Result<Handles, Error> {
    let mut handles = Handles::default();
    let of_type = |phid_type: PhidType| -> Vec<Phid> {
        let mut found: Vec<Phid> = phids
            .iter()
            .map(Phid::new)
            .filter(|phid| phid.phid_type() == phid_type)
            .collect();
        found.sort();
        found.dedup();
        found
    };

    let users = of_type(PhidType::USER);
    for user in phab_auth::user::load_users_by_phid(database, &users).await? {
        let username = user.user_name.clone();
        handles.handles.insert(
            user.phid.as_str().to_owned(),
            Handle {
                name: username.clone(),
                link_name: username.clone(),
                command_line_name: username.clone(),
                mail_stamp_name: Some(format!("_@{username}")),
                object_name: username,
                complete: true,
            },
        );
    }
    for phid in &users {
        handles
            .handles
            .entry(phid.as_str().to_owned())
            .or_insert_with(|| Handle::unknown("User"));
    }

    load_projects(
        database,
        config,
        viewer,
        &of_type(PhidType::PROJECT),
        now,
        &mut handles,
    )
    .await?;

    let repositories: Vec<&str> = phids
        .iter()
        .map(String::as_str)
        .filter(|phid| Phid::new(*phid).phid_type() == PhidType::REPOSITORY)
        .collect();
    for repository in load_repositories(database, &repositories).await? {
        handles.handles.insert(
            repository.phid.clone(),
            Handle {
                name: repository.monogram.clone(),
                link_name: format!("{} {}", repository.monogram, repository.name),
                command_line_name: repository.monogram.clone(),
                mail_stamp_name: Some(repository.monogram.clone()),
                object_name: repository.monogram,
                complete: true,
            },
        );
    }
    for phid in repositories {
        handles
            .handles
            .entry(phid.to_owned())
            .or_insert_with(|| Handle::unknown("Repository"));
    }

    if phids.iter().any(|phid| phid == HERALD_APPLICATION) {
        let mut herald = Handle::named("Herald".to_owned());
        herald.complete = true;
        handles
            .handles
            .insert(HERALD_APPLICATION.to_owned(), herald);
    }
    Ok(handles)
}
