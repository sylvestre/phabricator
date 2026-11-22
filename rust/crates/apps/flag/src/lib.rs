//! Methods of `PhabricatorFlagsApplication`.

pub mod edit;
pub mod query;

use phab_conduit::{Application, Registry};

/// `PhabricatorFlagsApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorFlagsApplication",
    name: "Flags",
};

/// Registers the Flags methods.
pub fn register(registry: &mut Registry) {
    registry.register(edit::FlagEdit);
    registry.register(edit::FlagDelete);
    registry.register(query::FlagQuery);
}
