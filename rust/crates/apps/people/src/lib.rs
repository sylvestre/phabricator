//! Methods of `PhabricatorPeopleApplication`.

pub mod profile_image;
pub mod whoami;

use phab_conduit::{Application, Registry};

/// `PhabricatorPeopleApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorPeopleApplication",
    name: "People",
};

/// Registers the People methods.
pub fn register(registry: &mut Registry) {
    registry.register(whoami::UserWhoAmI);
}
