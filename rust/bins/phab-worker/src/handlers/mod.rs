//! The task classes ported to Rust.

use crate::handler::Registry;

pub mod feed_publisher;
pub mod test_worker;

/// Every ported class.
pub fn registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(feed_publisher::FeedPublisherWorker);
    registry.register(test_worker::TestWorker);
    registry
}
