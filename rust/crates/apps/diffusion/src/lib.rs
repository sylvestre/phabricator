//! Methods of `PhabricatorDiffusionApplication`.

pub mod audit_query;
pub mod commit_search;
pub mod internal_commit_search;
pub mod legacy_query;
pub mod query_commits;
pub mod repository_search;

use phab_conduit::{Application, Registry};

/// `PhabricatorDiffusionApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorDiffusionApplication",
    name: "Diffusion",
};

/// Registers the Diffusion methods.
pub fn register(registry: &mut Registry) {
    registry.register(audit_query::AuditQuery);
    registry.register(commit_search::CommitSearch);
    registry.register(internal_commit_search::InternalCommitSearch);
    registry.register(legacy_query::LegacyQuery);
    registry.register(query_commits::QueryCommits);
    registry.register(repository_search::RepositorySearch);
}
