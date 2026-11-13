//! Handles for feed titles (`PhabricatorHandleQuery` as the viewer): what a
//! title shows of the users, revisions and projects it names.

use std::collections::HashMap;

use phab_app_differential::storage;
use phab_app_project as project;
use phab_core::{Config, Phid, PhidType};
use phab_db::Database;
use phab_policy::Viewer;

use crate::story::StoryError;

/// What a title shows of an object (`PhabricatorObjectHandle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// `getLinkName()`: the username for users, the full name otherwise.
    pub name: String,
    /// `getURI()`; `None` for objects the viewer can't see or which don't
    /// exist.
    pub uri: Option<String>,
}

impl Handle {
    /// A handle for an object the viewer may not see: "Restricted User".
    fn restricted(type_name: &str) -> Self {
        Self {
            name: format!("Restricted {type_name}"),
            uri: None,
        }
    }

    /// A handle for an object which doesn't exist.
    fn unknown(type_name: &str) -> Self {
        Self {
            name: format!("Unknown Object ({type_name})"),
            uri: None,
        }
    }
}

/// `getTypeName()` of the PHID types titles name.
fn type_name(phid_type: PhidType) -> Option<&'static str> {
    match phid_type {
        PhidType::USER => Some("User"),
        PhidType::REVISION => Some("Differential Revision"),
        PhidType::PROJECT => Some("Project"),
        _ => None,
    }
}

/// Whether [`load_handles()`] knows the type of this object.
pub fn is_supported(phid: &Phid) -> bool {
    type_name(phid.phid_type()).is_some()
}

fn by_type(phids: &[Phid], phid_type: PhidType) -> Vec<Phid> {
    let mut found: Vec<Phid> = phids
        .iter()
        .filter(|phid| phid.phid_type() == phid_type)
        .cloned()
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The handles of `phids` as `viewer` sees them. Objects of other types
/// than users, revisions and projects are [`StoryError::Unsupported`].
pub async fn load_handles(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    phids: &[Phid],
    now: i64,
) -> Result<HashMap<Phid, Handle>, StoryError> {
    if !phids.iter().all(is_supported) {
        return Err(StoryError::Unsupported(
            "feed title names an object whose handle is not ported",
        ));
    }
    let mut handles = HashMap::new();

    // Users can be seen by everyone who can log in.
    for user in
        phab_auth::user::load_users_by_phid(database, &by_type(phids, PhidType::USER)).await?
    {
        handles.insert(
            user.phid.clone(),
            Handle {
                uri: Some(format!("/p/{}/", user.user_name)),
                name: user.user_name,
            },
        );
    }

    let revisions = storage::load_revisions(database, &by_type(phids, PhidType::REVISION)).await?;
    let loaded: Vec<Phid> = revisions
        .iter()
        .map(|revision| revision.phid.clone())
        .collect();
    let visible = storage::visible_revisions(database, config, viewer, revisions, now)
        .await
        .map_err(|error| match error {
            storage::VisibilityError::Loader(error) => StoryError::Loader(error),
            storage::VisibilityError::Policy(error) => StoryError::Policy(error),
        })?;
    for revision in visible {
        handles.insert(
            revision.phid.clone(),
            Handle {
                name: format!("D{}: {}", revision.id, revision.title),
                uri: Some(format!("/D{}", revision.id)),
            },
        );
    }
    for phid in loaded {
        handles
            .entry(phid)
            .or_insert_with(|| Handle::restricted("Differential Revision"));
    }

    let projects =
        project::storage::load_by_phid(database, &by_type(phids, PhidType::PROJECT)).await?;
    let loaded: Vec<Phid> = projects
        .iter()
        .map(|project| project.phid.clone())
        .collect();
    let (visible, graph) =
        project::policy::visible_projects(database, config, viewer, projects, now)
            .await
            .map_err(|error| match error {
                project::policy::VisibilityError::Database(error) => StoryError::Database(error),
                project::policy::VisibilityError::Loader(error) => StoryError::Loader(error),
                project::policy::VisibilityError::Policy(error) => StoryError::Policy(error),
            })?;
    for project in &visible {
        handles.insert(
            project.phid.clone(),
            Handle {
                name: project.display_name(graph.parent(project)),
                uri: Some(project.uri()),
            },
        );
    }
    for phid in loaded {
        handles
            .entry(phid)
            .or_insert_with(|| Handle::restricted("Project"));
    }

    for phid in phids {
        if !handles.contains_key(phid) {
            let name = type_name(phid.phid_type()).expect("types are checked above");
            handles.insert(phid.clone(), Handle::unknown(name));
        }
    }
    Ok(handles)
}
