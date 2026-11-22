//! `PhabricatorFeedApplication`: the `feed.query_id` method, Mozilla's
//! `feed.for_email.*` methods, and the stories with their titles for the
//! feed pages.

pub mod email;
pub mod handles;
pub mod listing;
pub mod push;
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
    registry.register(email::FeedForEmailQuery);
    registry.register(email::FeedForEmailQueryId);
    registry.register(email::FeedForEmailStatus);
    registry.register(email::FeedForEmailStatusId);
}
