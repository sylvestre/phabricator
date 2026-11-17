//! Differential objects, loaded the way the PHP queries load them and
//! described for the policy filter.

pub mod buckets;
pub mod changeset_search;
pub mod commit;
pub mod details;
pub mod diff_search;
pub mod edge_search;
pub mod feed;
pub mod repository;
pub mod search;
pub mod storage;
pub mod title;
pub mod transaction_search;

pub use storage::{Comment, Repositories, Revision, Transaction};

/// Registers the Differential methods.
pub fn register(registry: &mut phab_conduit::Registry) {
    registry.register(search::RevisionSearch);
    registry.register(diff_search::DiffSearch);
    registry.register(changeset_search::ChangesetSearch);
    registry.register(edge_search::EdgeSearch);
    registry.register(transaction_search::TransactionSearch);
}
