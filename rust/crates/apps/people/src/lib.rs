//! Methods of `PhabricatorPeopleApplication`.

pub mod profile_image;
pub mod whoami;

use phab_conduit::{Application, Registry};

pub const APPLICATION: Application = Application {
    class: "PhabricatorPeopleApplication",
    name: "People",
};

pub fn register(registry: &mut Registry) {
    registry.register(whoami::UserWhoAmI);
}
