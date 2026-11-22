//! Differential diffs as PHP stores and aligns them: diffs, changesets and
//! hunks from the database ([`storage`]), the line alignment of
//! `DifferentialHunkParser` ([`parser`]) and the intraline changes of
//! `ArcanistDiffUtils` ([`intraline`]).
//!
//! The output must match PHP exactly: the same changed lines, the same
//! alignment and the same intraline ranges, so a review shows the same
//! changes in both interfaces.

pub mod arcanist;
pub mod build;
mod changeset;
pub mod deflate;
pub mod edit_distance;
pub mod git_patch;
mod hunk;
pub mod intraline;
pub mod lines;
pub mod parser;
pub mod realign;
pub mod storage;
pub mod unified;
pub mod view;

pub use changeset::{ChangeType, Changeset, FileType, sort_for_display};
pub use hunk::{Hunk, Include};
