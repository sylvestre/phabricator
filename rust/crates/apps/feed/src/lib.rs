//! Methods of `PhabricatorFeedApplication`.

pub mod query_id;
pub mod story;

use phab_conduit::{Application, Registry};

pub const APPLICATION: Application = Application {
    class: "PhabricatorFeedApplication",
    name: "Feed",
};

pub fn register(registry: &mut Registry) {
    registry.register(query_id::FeedQueryId);
}
