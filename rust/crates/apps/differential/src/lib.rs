//! Differential objects, loaded the way the PHP queries load them and
//! described for the policy filter.

pub mod buckets;
pub mod changeset_search;
pub mod commit;
pub mod details;
pub mod diff_search;
pub mod edge_search;
pub mod feed;
pub mod get_commit_paths;
pub mod get_raw_diff;
pub mod legacy_query;
pub mod query_diffs;
pub mod repository;
pub mod revision_comments;
pub mod search;
pub mod storage;
pub mod title;
pub mod transaction_search;
pub mod visibility;

pub use storage::{Comment, Repositories, Revision, Transaction};

/// Registers the Differential methods.
pub fn register(registry: &mut phab_conduit::Registry) {
    registry.register(search::RevisionSearch);
    registry.register(diff_search::DiffSearch);
    registry.register(changeset_search::ChangesetSearch);
    registry.register(query_diffs::QueryDiffs);
    registry.register(query_diffs::GetAllDiffs);
    registry.register(query_diffs::GetDiff);
    registry.register(get_raw_diff::GetRawDiff);
    registry.register(get_commit_paths::GetCommitPaths);
    registry.register(legacy_query::LegacyQuery);
    registry.register(legacy_query::GetRevision);
    registry.register(edge_search::EdgeSearch);
    registry.register(transaction_search::TransactionSearch);
    registry.register(revision_comments::GetRevisionComments);
    registry.register(visibility::RevisionVisibility);
}
