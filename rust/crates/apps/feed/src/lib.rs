//! Methods of `PhabricatorFeedApplication`.

pub mod query_id;
pub mod story;

use phab_conduit::{Application, Registry};

/// `PhabricatorFeedApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorFeedApplication",
    name: "Feed",
};

/// Registers the Feed methods.
pub fn register(registry: &mut Registry) {
    registry.register(query_id::FeedQueryId);
}
