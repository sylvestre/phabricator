//! Differential objects, loaded the way the PHP queries load them and
//! described for the policy filter.

pub mod feed;
pub mod storage;

pub use storage::{Comment, Repositories, Revision, Transaction};
