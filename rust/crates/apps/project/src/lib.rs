//! Projects (`PhabricatorProject`): loading them by PHID or hashtag, and
//! deciding which ones a viewer can see, which depends on their parents
//! and on membership; creating and editing them.

pub mod create;
pub mod edit;
mod editor;
pub mod legacy_query;
pub mod policy;
pub mod search;
pub mod storage;

pub use storage::Project;

/// Registers the Projects methods.
pub fn register(registry: &mut phab_conduit::Registry) {
    registry.register(create::ProjectCreate);
    registry.register(edit::ProjectEdit);
    registry.register(legacy_query::LegacyQuery);
    registry.register(search::ProjectSearch);
}
