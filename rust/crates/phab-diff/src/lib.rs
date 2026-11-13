//! Differential diffs as PHP stores and aligns them: hunk storage
//! (`DifferentialHunk`), the line alignment of `DifferentialHunkParser`
//! and the intraline changes of `ArcanistDiffUtils`.
//!
//! The output must match PHP exactly: the same changed lines, the same
//! alignment and the same intraline ranges, so a review shows the same
//! changes in both interfaces.

pub mod edit_distance;
mod hunk;
pub mod intraline;
pub mod lines;
pub mod parser;
pub mod unified;

pub use hunk::{Hunk, Include};
