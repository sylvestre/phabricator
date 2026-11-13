//! Which projects a viewer can see (`PhabricatorProjectQuery` and the
//! policy methods of `PhabricatorProject`):
//!
//! - milestones have their parent's policies;
//! - members can always view, and members of a subproject count as members
//!   of its ancestors;
//! - viewing a subproject or a milestone also needs viewing its parent;
//! - a project whose ancestors can't be loaded is dropped.

use std::collections::{HashMap, HashSet};

use phab_core::{Config, Phid};
use phab_db::{Database, edges};
use phab_policy::loader::{self, LoaderError, SubjectResolver};
use phab_policy::{
    AutomaticCapability, Capability, ExtendedPolicy, ObjectKind, PolicySubject, Viewer,
};

use crate::storage::{self, Project};

/// How many levels of ancestors are loaded; Phabricator allows 16.
const MAX_DEPTH: usize = 16;

/// Why [`visible_projects()`] failed.
#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    /// A query failed.
    #[error(transparent)]
    Database(#[from] phab_db::DbError),
    /// Loading the policy facts failed.
    #[error(transparent)]
    Loader(#[from] LoaderError),
    /// The policy filter refused the check.
    #[error(transparent)]
    Policy(#[from] phab_policy::PolicyError),
}

/// The projects and all their ancestors, by PHID.
pub struct Graph {
    projects: HashMap<Phid, Project>,
}

impl Graph {
    /// Loads the ancestors of `projects`.
    pub async fn load(database: &Database, projects: &[Project]) -> Result<Self, phab_db::DbError> {
        let mut graph: HashMap<Phid, Project> = projects
            .iter()
            .map(|project| (project.phid.clone(), project.clone()))
            .collect();
        for _ in 0..MAX_DEPTH {
            let missing: Vec<Phid> = graph
                .values()
                .filter_map(|project| project.parent_phid.clone())
                .filter(|parent| !graph.contains_key(parent))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            if missing.is_empty() {
                break;
            }
            for parent in storage::load_by_phid(database, &missing).await? {
                graph.insert(parent.phid.clone(), parent);
            }
        }
        Ok(Self { projects: graph })
    }

    /// A project of the graph.
    pub fn get(&self, phid: &Phid) -> Option<&Project> {
        self.projects.get(phid)
    }

    /// The parent of a project, if it has one and it was loaded.
    pub fn parent(&self, project: &Project) -> Option<&Project> {
        project
            .parent_phid
            .as_ref()
            .and_then(|parent| self.projects.get(parent))
    }

    /// The project and its ancestors, or `None` if one is missing (a
    /// damaged graph, see T13484) or the chain loops.
    fn ancestry(&self, project: &Project) -> Option<Vec<Phid>> {
        let mut chain = vec![project.phid.clone()];
        let mut current = project;
        while let Some(parent) = &current.parent_phid {
            if chain.contains(parent) || chain.len() > MAX_DEPTH {
                return None;
            }
            current = self.projects.get(parent)?;
            chain.push(parent.clone());
        }
        Some(chain)
    }

    /// `PhabricatorProject` as a policy object for `viewer`, who is a
    /// member if they are in `members`.
    fn subject(
        &self,
        project: &Project,
        viewer: &Viewer,
        members: &HashSet<Phid>,
    ) -> PolicySubject {
        // Milestones take their policies and members from their parent.
        let owner = if project.is_milestone() {
            self.parent(project).unwrap_or(project)
        } else {
            project
        };
        let mut subject = PolicySubject::new(Some(project.phid.clone()), ObjectKind::Project)
            .with_policy(Capability::VIEW, owner.view_policy.clone())
            .with_policy(Capability::EDIT, owner.edit_policy.clone())
            .with_policy(Capability::JOIN, owner.join_policy.clone());
        subject.capabilities.push(Capability::JOIN);
        subject.space = Some(project.space_phid.clone());
        if let Some(viewer_phid) = &viewer.phid
            && members.contains(&owner.phid)
        {
            subject.automatic.push(AutomaticCapability {
                principal: viewer_phid.clone(),
                capabilities: Some(vec![Capability::VIEW]),
            });
        }
        if let Some(parent) = &project.parent_phid {
            subject.extended.push(ExtendedPolicy {
                for_capability: Capability::VIEW,
                object: parent.clone(),
                capabilities: vec![Capability::VIEW],
            });
        }
        subject
    }

    /// The projects `viewer` is a member of: those they are a materialized
    /// member of, and all their ancestors.
    async fn members(
        &self,
        database: &Database,
        viewer: &Viewer,
    ) -> Result<HashSet<Phid>, phab_db::DbError> {
        let Some(viewer_phid) = &viewer.phid else {
            return Ok(HashSet::new());
        };
        let sources: Vec<Phid> = self.projects.keys().cloned().collect();
        let mut members = HashSet::new();
        for edge in edges::load_edges(
            database,
            &sources,
            &[edges::PROJECT_MATERIALIZED_MEMBER],
            Some(std::slice::from_ref(viewer_phid)),
        )
        .await?
        {
            let Some(project) = self.projects.get(&edge.src) else {
                continue;
            };
            if let Some(chain) = self.ancestry(project) {
                members.extend(chain);
            }
        }
        Ok(members)
    }
}

/// Resolves the parents named by extended policies from the loaded graph.
struct Parents<'a> {
    subjects: &'a HashMap<Phid, PolicySubject>,
}

impl SubjectResolver for Parents<'_> {
    fn resolve(
        &self,
        phids: &[Phid],
    ) -> impl Future<Output = Result<HashMap<Phid, PolicySubject>, LoaderError>> + Send {
        std::future::ready(Ok(phids
            .iter()
            .filter_map(|phid| Some((phid.clone(), self.subjects.get(phid)?.clone())))
            .collect()))
    }
}

/// The projects `viewer` can see, with the graph of their ancestors.
pub async fn visible_projects(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    projects: Vec<Project>,
    now: i64,
) -> Result<(Vec<Project>, Graph), VisibilityError> {
    let graph = Graph::load(database, &projects).await?;
    let members = graph.members(database, viewer).await?;
    let projects: Vec<Project> = projects
        .into_iter()
        .filter(|project| graph.ancestry(project).is_some())
        .collect();
    let all_subjects: HashMap<Phid, PolicySubject> = graph
        .projects
        .values()
        .map(|project| {
            (
                project.phid.clone(),
                graph.subject(project, viewer, &members),
            )
        })
        .collect();
    let subjects: Vec<&PolicySubject> = projects
        .iter()
        .map(|project| &all_subjects[&project.phid])
        .collect();
    let resolver = Parents {
        subjects: &all_subjects,
    };
    let facts = loader::load_facts(database, config, viewer, &subjects, &resolver, now).await?;
    let passed = phab_policy::filter(viewer, &subjects, &[Capability::VIEW], &facts)?;
    let visible = projects
        .into_iter()
        .zip(passed)
        .filter_map(|(project, visible)| visible.then_some(project))
        .collect();
    Ok((visible, graph))
}
