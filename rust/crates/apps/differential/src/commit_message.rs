//! Commit messages: `differential.getcommitmessage` renders a revision as
//! one, `differential.parsecommitmessage` reads the fields of one
//! (`DifferentialCommitMessageField` and `DifferentialCommitMessageParser`).

pub mod get;
pub mod parse;

mod fields;
mod objects;
