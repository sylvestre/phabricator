//! Differential objects, loaded the way the PHP queries load them and
//! described for the policy filter.

pub mod buckets;
pub mod changeset_search;
pub mod commit;
pub mod commit_message;
pub mod create_diff;
pub mod create_raw_diff;
pub mod details;
pub(crate) mod diff_editor;
pub mod diff_search;
pub mod edge_search;
pub mod edit;
pub mod feed;
pub mod get_commit_paths;
pub mod get_raw_diff;
pub mod legacy_query;
pub mod markup;
pub mod query_diffs;
pub mod repository;
pub mod revision_comments;
pub mod search;
pub mod set_diff_property;
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
    registry.register(edit::RevisionEdit::default());
    registry.register(transaction_search::TransactionSearch);
    registry.register(revision_comments::GetRevisionComments);
    registry.register(visibility::RevisionVisibility);
    registry.register(commit_message::get::GetCommitMessage);
    registry.register(commit_message::parse::ParseCommitMessage);
    registry.register(create_diff::CreateDiff);
    registry.register(create_raw_diff::CreateRawDiff);
    registry.register(set_diff_property::SetDiffProperty);
}
