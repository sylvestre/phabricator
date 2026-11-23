//! Methods of `PhabricatorDiffusionApplication`.

pub mod audit_query;
pub mod blame;
pub mod branch_query;
pub mod browse_query;
pub mod commit_parents_query;
pub mod commit_search;
pub mod drequest;
pub mod exists_query;
pub mod git;
pub mod history;
pub mod history_query;
pub mod import;
pub mod internal_ancestors;
pub mod internal_commit_search;
pub mod internal_git_raw_diff_query;
pub mod last_modified_query;
pub mod legacy_query;
pub mod lint_messages;
pub mod merged_commits_query;
pub mod php;
pub mod query;
pub mod query_commits;
pub mod query_paths;
pub mod recent_commits_by_path;
pub mod refs;
pub mod refs_query;
pub mod repository_search;
pub mod resolve_refs;
pub mod search_query;
pub mod tags_query;

use phab_conduit::{Application, Registry};

use crate::query::Query;

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
    registry.register(lint_messages::GetLintMessages);
    registry.register(recent_commits_by_path::GetRecentCommitsByPath);
    registry.register(Query(blame::Blame));
    registry.register(Query(branch_query::BranchQuery));
    registry.register(Query(browse_query::BrowseQuery));
    registry.register(Query(commit_parents_query::CommitParentsQuery));
    registry.register(Query(exists_query::ExistsQuery));
    registry.register(Query(history_query::HistoryQuery));
    registry.register(Query(internal_ancestors::InternalAncestors));
    registry.register(Query(internal_git_raw_diff_query::InternalGitRawDiffQuery));
    registry.register(Query(last_modified_query::LastModifiedQuery));
    registry.register(Query(merged_commits_query::MergedCommitsQuery));
    registry.register(Query(query_paths::QueryPaths));
    registry.register(Query(refs_query::RefsQuery));
    registry.register(Query(resolve_refs::ResolveRefs));
    registry.register(Query(search_query::SearchQuery));
    registry.register(Query(tags_query::TagsQuery));
}
