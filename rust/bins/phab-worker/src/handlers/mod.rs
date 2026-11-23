//! The task classes ported to Rust.

use crate::handler::Registry;

pub mod feed_publisher;
pub mod harbormaster;
pub mod metamta;
pub mod publish;
pub mod repository;
pub mod search;
pub mod test_worker;
pub mod update_after_commit;

/// Every ported class.
pub fn registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(feed_publisher::FeedPublisherWorker);
    registry.register(harbormaster::BuildWorker);
    registry.register(harbormaster::TargetWorker::default());
    registry.register(metamta::MetaMTAWorker::default());
    registry.register(publish::PublishWorker);
    registry.register(repository::MessageParserWorker);
    registry.register(repository::ChangeParserWorker);
    registry.register(repository::PublishWorker::from_env());
    registry.register(search::SearchWorker);
    registry.register(test_worker::TestWorker);
    registry.register(update_after_commit::UpdateObjectAfterCommitWorker);
    registry
}
