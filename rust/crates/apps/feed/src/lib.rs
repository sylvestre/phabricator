//! `PhabricatorFeedApplication`: the `feed.query` and `feed.query_id`
//! methods, Mozilla's `feed.for_email.*` methods, the stories with their
//! titles for the feed pages, and the notifications of the notification
//! page.

pub mod email;
pub mod handles;
pub mod listing;
pub mod push;
pub mod query;
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
    registry.register(query::FeedQuery);
    registry.register(query_id::FeedQueryId);
    registry.register(email::FeedForEmailQuery);
    registry.register(email::FeedForEmailQueryId);
    registry.register(email::FeedForEmailStatus);
    registry.register(email::FeedForEmailStatusId);
}
