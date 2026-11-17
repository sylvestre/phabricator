//! Projects (`PhabricatorProject`): loading them by PHID or hashtag, and
//! deciding which ones a viewer can see, which depends on their parents
//! and on membership.

pub mod policy;
pub mod search;
pub mod storage;

pub use storage::Project;

/// Registers the Projects methods.
pub fn register(registry: &mut phab_conduit::Registry) {
    registry.register(search::ProjectSearch);
}
