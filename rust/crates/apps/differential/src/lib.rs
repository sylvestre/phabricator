//! Differential objects, loaded the way the PHP queries load them and
//! described for the policy filter.

pub mod buckets;
pub mod commit;
pub mod details;
pub mod feed;
pub mod repository;
pub mod search;
pub mod storage;
pub mod title;

pub use storage::{Comment, Repositories, Revision, Transaction};

/// Registers the Differential methods.
pub fn register(registry: &mut phab_conduit::Registry) {
    registry.register(search::RevisionSearch);
}
