//! `PhabricatorHandleQuery` for the `phid.*` methods: the handles of the
//! objects the viewer can see, for users, revisions, diffs, projects and
//! repositories.

use std::collections::HashMap;

use phab_app_differential::{diff_search, repository, storage};
use phab_app_project as project;
use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, PhidType};
use serde_json::{Value, json};

/// A complete handle (`PhabricatorObjectHandle`), of an object the viewer
/// can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// The object's PHID.
    pub phid: Phid,
    /// `getURI()`, a path.
    pub uri: String,
    /// `getName()`.
    pub name: String,
    /// `getFullName()`.
    pub full_name: String,
    /// `getStatus() == STATUS_CLOSED`.
    pub closed: bool,
}

/// `getTypeName()` of the types whose handles are ported.
fn type_name(phid_type: PhidType) -> Option<&'static str> {
    match phid_type {
        PhidType::USER => Some("User"),
        PhidType::REVISION => Some("Differential Revision"),
        PhidType::DIFF => Some("Differential Diff"),
        PhidType::PROJECT => Some("Project"),
        PhidType::REPOSITORY => Some("Repository"),
        _ => None,
    }
}

/// Whether [`load()`] knows the handles of objects of this PHID.
pub fn is_supported(phid: &Phid) -> bool {
    type_name(phid.phid_type()).is_some()
}

impl Handle {
    /// `buildHandleInformationDictionary()`.
    pub fn dictionary(&self, production_uri: &str) -> Value {
        let phid_type = self.phid.phid_type();
        json!({
            "phid": self.phid.as_str(),
            "uri": format!("{}{}", production_uri.trim_end_matches('/'), self.uri),
            "typeName": type_name(phid_type),
            "type": phid_type.as_str(),
            "name": self.name,
            "fullName": self.full_name,
            "status": if self.closed { "closed" } else { "open" },
        })
    }
}

fn of_type(phids: &[Phid], phid_type: PhidType) -> Vec<Phid> {
    let mut found: Vec<Phid> = phids
        .iter()
        .filter(|phid| phid.phid_type() == phid_type)
        .cloned()
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The user handle: `alice (Alice Liddell)`, at `/p/alice/`.
pub fn user_handle(user: &phab_auth::user::User) -> Handle {
    let full_name = if user.real_name.is_empty() {
        user.user_name.clone()
    } else {
        format!("{} ({})", user.user_name, user.real_name)
    };
    Handle {
        phid: user.phid.clone(),
        uri: format!("/p/{}/", user.user_name),
        name: user.user_name.clone(),
        full_name,
        closed: false,
    }
}

/// The revision handle: `D12: Title`, at `/D12`.
pub fn revision_handle(revision: &storage::Revision) -> Handle {
    let monogram = format!("D{}", revision.id);
    Handle {
        phid: revision.phid.clone(),
        uri: format!("/{monogram}"),
        name: monogram.clone(),
        full_name: format!("{monogram}: {}", revision.title),
        closed: revision.is_closed(),
    }
}

/// The project handle: its display name, at its hashtag.
pub fn project_handle(project: &project::Project, parent: Option<&project::Project>) -> Handle {
    let name = project.display_name(parent);
    Handle {
        phid: project.phid.clone(),
        uri: project.uri(),
        name: name.clone(),
        full_name: name,
        closed: project.is_archived(),
    }
}

/// The repository handle: `rMOZ Name`; closed unless active.
pub fn repository_handle(repository: &repository::Repository) -> Handle {
    let monogram = repository.monogram();
    Handle {
        phid: repository.phid.clone(),
        uri: repository.uri(),
        name: monogram.clone(),
        full_name: format!("{monogram} {}", repository.name),
        closed: !repository.is_tracked(),
    }
}

/// The handles of the objects of `phids` the viewer can see. PHIDs of
/// types whose handles are not ported are [`MethodError::Unsupported`].
pub async fn load(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<HashMap<Phid, Handle>, MethodError> {
    if !phids.iter().all(is_supported) {
        return Err(MethodError::Unsupported("handles of other objects"));
    }
    let context = request.context;
    let database = &context.database;
    let config = &context.config;
    let viewer = &request.viewer;
    let mut handles = HashMap::new();

    // Users are visible to everyone who can call Conduit.
    for user in
        phab_auth::user::load_users_by_phid(database, &of_type(phids, PhidType::USER)).await?
    {
        handles.insert(user.phid.clone(), user_handle(&user));
    }

    let revisions = storage::load_revisions(database, &of_type(phids, PhidType::REVISION)).await?;
    for revision in storage::visible_revisions(database, config, viewer, revisions, request.now)
        .await
        .map_err(MethodError::internal)?
    {
        handles.insert(revision.phid.clone(), revision_handle(&revision));
    }

    for (id, phid) in diff_search::visible_diffs(request, &of_type(phids, PhidType::DIFF)).await? {
        let name = format!("Diff {id}");
        handles.insert(
            phid.clone(),
            Handle {
                phid,
                uri: format!("/differential/diff/{id}/"),
                name: name.clone(),
                full_name: name,
                closed: false,
            },
        );
    }

    let projects =
        project::storage::load_by_phid(database, &of_type(phids, PhidType::PROJECT)).await?;
    if !projects.is_empty() {
        let (visible, graph) =
            project::policy::visible_projects(database, config, viewer, projects, request.now)
                .await
                .map_err(MethodError::internal)?;
        for project in &visible {
            handles.insert(
                project.phid.clone(),
                project_handle(project, graph.parent(project)),
            );
        }
    }

    let repositories = repository::load(database, &of_type(phids, PhidType::REPOSITORY)).await?;
    for repository in repository::visible(database, config, viewer, repositories, request.now)
        .await
        .map_err(MethodError::internal)?
    {
        handles.insert(repository.phid.clone(), repository_handle(&repository));
    }
    Ok(handles)
}
