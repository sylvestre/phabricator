//! The task classes ported to Rust.

use crate::handler::Registry;

pub mod feed_publisher;
pub mod metamta;
pub mod publish;
pub mod repository;
pub mod test_worker;

/// Every ported class.
pub fn registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(feed_publisher::FeedPublisherWorker);
    registry.register(metamta::MetaMTAWorker::default());
    registry.register(publish::PublishWorker);
    registry.register(repository::MessageParserWorker);
    registry.register(repository::ChangeParserWorker);
    registry.register(repository::PublishWorker::from_env());
    registry.register(test_worker::TestWorker);
    registry
}
