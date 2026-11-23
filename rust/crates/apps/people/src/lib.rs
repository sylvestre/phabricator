//! Methods of `PhabricatorPeopleApplication`.

pub mod bugzilla;
pub mod edit;
mod editor;
pub mod external_account;
pub mod find;
pub mod profile;
pub mod profile_image;
pub mod query;
pub mod search;
pub mod whoami;

use phab_conduit::{Application, Registry};

pub use editor::can_disable_users;

/// `PhabricatorPeopleApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorPeopleApplication",
    name: "People",
};

/// Registers the People methods.
pub fn register(registry: &mut Registry) {
    registry.register(bugzilla::BugzillaAccountSearch);
    registry.register(bugzilla::BmoExternalAccountSearch);
    registry.register(edit::UserDisable);
    registry.register(edit::UserEdit);
    registry.register(edit::UserEnable);
    registry.register(find::UserFind);
    registry.register(query::UserQuery);
    registry.register(search::UserSearch);
    registry.register(whoami::UserWhoAmI);
}
