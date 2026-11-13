//! What a changeset shows, without HTML: the work of
//! `DifferentialChangesetParser::process()` and `render()` for a top-level
//! render, up to `DifferentialChangesetRenderer::buildPrimitives()`.
//!
//! A unified or side-by-side renderer turns the [`Row`]s into markup and
//! applies syntax highlighting; the changed lines, their alignment, the
//! intraline ranges, the gaps and the shields are decided here, as PHP
//! decides them.
//!
//! Not ported: inline comments (they add rows to the mask; see
//! [`ViewOptions::feedback_rows`]), document engines (images, Jupyter
//! notebooks), "show more" range requests and syntax highlighting.

use std::collections::BTreeSet;

use serde_json::Value;
use thiserror::Error;

use crate::changeset::{ChangeType, Changeset, FileType};
use crate::intraline::Segment;
use crate::parser::{DepthChange, HunkParser, Line, LineType, ParseError};
use crate::realign::{self, DiffCommand, RealignError};

/// `DifferentialChangesetParser::$linesOfContext`.
pub const DEFAULT_LINES_OF_CONTEXT: usize = 8;
/// Above this many changed lines, the file is behind a shield.
const LARGE_CHANGE_LINES: i64 = 2500;

/// How to build the view.
#[derive(Debug, Clone)]
pub struct ViewOptions {
    /// Lines of context around changes.
    pub lines_context: usize,
    /// Whether `differential.generated-paths` matches the file name; the
    /// patterns are PCRE, so the caller matches them.
    pub path_is_generated: bool,
    /// Whether the changeset has inline comments that are not collapsed:
    /// shields are not shown then.
    pub has_expanded_comments: bool,
    /// Rows shown because of inline comments (`$feedback_mask`).
    pub feedback_rows: BTreeSet<usize>,
    /// The command for the whitespace-insensitive rediff.
    pub diff_command: DiffCommand,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            lines_context: DEFAULT_LINES_OF_CONTEXT,
            path_is_generated: false,
            has_expanded_comments: false,
            feedback_rows: BTreeSet::new(),
            diff_command: DiffCommand::default(),
        }
    }
}

/// Why a changeset could not be viewed; PHP fails on these too.
#[derive(Debug, Error)]
pub enum ViewError {
    /// The hunks are malformed.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// The whitespace-insensitive rediff failed.
    #[error(transparent)]
    Realign(#[from] RealignError),
}

/// Why the diff of a file is hidden behind a shield (`renderShield()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shield {
    /// "This file contains generated code, which does not normally need to
    /// be reviewed."
    Generated,
    /// An empty shield: the old location of a moved file, whose move is
    /// shown in the header.
    MoveAway,
    /// "The contents of this file were not changed.", or "This is an empty
    /// file." for an added file. `has_content` is false when the diff has
    /// no lines to reveal (the shield's force mode `none`).
    Unchanged {
        /// The file was added (`TYPE_ADD`).
        empty_file: bool,
        /// The text can be revealed.
        has_content: bool,
    },
    /// "This file was completely deleted."
    Deleted,
    /// "This file has a very large number of changes (N lines)."
    TooLarge {
        /// `getAffectedLineCount()`.
        lines: i64,
    },
}

/// One side of a displayed line (`buildPrimitives()`'s `$ospec` or
/// `$nspec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The line number on this side.
    pub line: i64,
    /// The change marker; `None` for context.
    pub kind: Option<LineType>,
    /// The text, with its line ending.
    pub text: String,
    /// Intraline segments, when both sides of the row differ.
    pub segments: Vec<Segment>,
    /// The row only changed indentation.
    pub depth_only: Option<DepthChange>,
}

/// A displayed row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A line, on one side or both.
    Line {
        /// The row index, used by "show more" requests.
        row: usize,
        /// The old side.
        old: Option<Cell>,
        /// The new side.
        new: Option<Cell>,
    },
    /// Hidden rows, shown as "show more context" (`'context'`).
    Gap {
        /// The first hidden row.
        top: usize,
        /// How many rows are hidden.
        length: usize,
    },
    /// "Context not available." between hunks (`'no-context'`).
    NoContext,
}

/// What the body of a changeset shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// The diff is hidden.
    Shielded(Shield),
    /// No text diff: directories, binary files and images (PHP renders
    /// images and some binaries with document engines, not ported).
    NoText,
    /// The rows of the text diff.
    Rows(Vec<Row>),
}

/// A changeset ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesetView {
    /// `getDisplayFilename()`.
    pub display_filename: String,
    /// `oldFile`, for moves and copies.
    pub old_file: Option<String>,
    /// `awayPaths`.
    pub away_paths: Vec<String>,
    /// What happened to the path.
    pub change_type: ChangeType,
    /// What kind of file it is.
    pub file_type: FileType,
    /// `metadata['old:binary-phid']`: the old file of a binary change.
    pub old_binary_phid: Option<String>,
    /// `metadata['new:binary-phid']`.
    pub new_binary_phid: Option<String>,
    /// The body.
    pub body: Body,
}

/// `isGeneratedChangeset()`: the `generated` attribute, trusted values
/// first.
fn has_generated_attribute(metadata: &Value) -> bool {
    ["attributes.trusted", "attributes.untrusted"]
        .iter()
        .find_map(|key| metadata.get(key)?.get("generated"))
        .is_some_and(php_truthy)
}

/// PHP truthiness of a decoded JSON value.
fn php_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty() && text != "0",
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata.get(key)?.as_str().map(str::to_owned)
}

/// `calculateGapsAndMask()` for a top-level render: the rows shown, and the
/// hidden runs longer than the context, earliest first.
fn gaps_and_mask(
    visible: &[bool],
    feedback: &BTreeSet<usize>,
    rows: usize,
    lines_context: usize,
) -> (Vec<(usize, usize)>, Vec<bool>) {
    let mut mask = vec![false; rows + 1];
    for (row, &shown) in visible.iter().enumerate() {
        mask[row] |= shown;
    }
    for &row in feedback {
        if row <= rows {
            mask[row] = true;
        }
    }
    mask[rows] = true;

    let mut gaps = Vec::new();
    let mut gap_start = 0;
    let mut in_gap = false;
    for row in 0..=rows {
        if mask[row] {
            if in_gap {
                let length = row - gap_start;
                if length <= lines_context {
                    for revealed in &mut mask[gap_start..=gap_start + length] {
                        *revealed = true;
                    }
                } else {
                    gaps.push((gap_start, length));
                }
                in_gap = false;
            }
        } else if !in_gap {
            gap_start = row;
            in_gap = true;
        }
    }
    (gaps, mask)
}

fn cell(
    line: Option<&Line>,
    segments: Option<&Vec<Segment>>,
    depth: Option<DepthChange>,
) -> Option<Cell> {
    line.map(|line| Cell {
        line: line.line,
        kind: line.kind,
        text: line.text.clone(),
        segments: segments.cloned().unwrap_or_default(),
        depth_only: depth,
    })
}

/// Builds the view of a changeset with its hunks. Runs the rediff of
/// [`realign`]: call it from a blocking task.
pub fn build(changeset: &Changeset, options: &ViewOptions) -> Result<ChangesetView, ViewError> {
    let mut parser = HunkParser::parse(&changeset.hunks)?;
    realign::realign(changeset, &mut parser, &options.diff_command)?;
    parser.reparse_for_special_attributes();

    let unchanged = !parser.has_any_changes()
        && matches!(changeset.file_type, FileType::Text | FileType::Symlink);
    let deleted = parser.is_deleted();
    let move_away = changeset.change_type == ChangeType::MoveAway;
    let intraline = parser.intraline_diffs();
    let visible = parser.visible_lines_mask(options.lines_context);
    let hunk_starts = HunkParser::hunk_start_lines(&changeset.hunks);
    let generated = parser.new_corpus().concat().contains("@generated")
        || options.path_is_generated
        || has_generated_attribute(&changeset.metadata);
    let rows = parser.old_lines.len().max(parser.new_lines.len());

    let shield = if options.has_expanded_comments {
        None
    } else if generated {
        Some(Shield::Generated)
    } else if move_away {
        Some(Shield::MoveAway)
    } else if unchanged {
        Some(Shield::Unchanged {
            empty_file: changeset.change_type == ChangeType::Add,
            has_content: rows != 0,
        })
    } else if deleted {
        Some(Shield::Deleted)
    } else if changeset.affected_line_count() > LARGE_CHANGE_LINES {
        Some(Shield::TooLarge {
            lines: changeset.affected_line_count(),
        })
    } else {
        None
    };

    let body = if let Some(shield) = shield {
        Body::Shielded(shield)
    } else if matches!(
        changeset.file_type,
        FileType::Directory | FileType::Binary | FileType::Image
    ) {
        Body::NoText
    } else {
        let (gaps, mask) = gaps_and_mask(
            &visible,
            &options.feedback_rows,
            rows,
            options.lines_context,
        );
        let mut gaps = gaps.into_iter();
        let mut output = Vec::new();
        let size = parser.old_lines.len();
        let mut row = 0;
        while row < rows {
            if !mask[row] {
                let Some((top, length)) = gaps.next() else {
                    break;
                };
                output.push(Row::Gap { top, length });
                row += length.max(1);
                continue;
            }
            let old = parser.old_lines.get(row).and_then(Option::as_ref);
            let new = parser.new_lines.get(row).and_then(Option::as_ref);
            let diff = intraline.diffs.get(&row);
            let depth = intraline.depth_only.get(&row).copied();
            if old.is_some_and(|old| hunk_starts.contains(&old.line)) {
                output.push(Row::NoContext);
            }
            output.push(Row::Line {
                row,
                old: cell(old, diff.map(|diff| &diff.old), depth),
                new: cell(new, diff.map(|diff| &diff.new), depth),
            });
            if !hunk_starts.is_empty() && row + 1 == size {
                output.push(Row::NoContext);
            }
            row += 1;
        }
        Body::Rows(output)
    };

    Ok(ChangesetView {
        display_filename: changeset.display_filename(),
        old_file: changeset.old_file.clone(),
        away_paths: changeset.away_paths.clone(),
        change_type: changeset.change_type,
        file_type: changeset.file_type,
        old_binary_phid: metadata_string(&changeset.metadata, "old:binary-phid"),
        new_binary_phid: metadata_string(&changeset.metadata, "new:binary-phid"),
        body,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn short_gaps_are_shown() {
        // Rows 1 and 9 visible, context 3: row 0 is revealed, the gap
        // 2..9 (7 rows) stays.
        let mut visible = vec![false; 10];
        visible[1] = true;
        visible[9] = true;
        let (gaps, mask) = gaps_and_mask(&visible, &BTreeSet::new(), 10, 3);
        assert_eq!(gaps, [(2, 7)]);
        assert!(mask[0] && mask[1] && mask[9] && !mask[5]);

        let (gaps, mask) = gaps_and_mask(&visible, &BTreeSet::new(), 10, 8);
        assert_eq!(gaps, []);
        assert!(mask.iter().all(|&shown| shown));
    }

    #[test]
    fn feedback_rows_are_shown() {
        let visible = vec![false; 30];
        let feedback = BTreeSet::from([15]);
        let (gaps, _) = gaps_and_mask(&visible, &feedback, 30, 2);
        assert_eq!(gaps, [(0, 15), (16, 14)]);
    }

    #[test]
    fn generated_attributes_prefer_trusted_values() {
        assert!(has_generated_attribute(
            &json!({"attributes.trusted": {"generated": true}})
        ));
        assert!(!has_generated_attribute(&json!({
            "attributes.trusted": {"generated": false},
            "attributes.untrusted": {"generated": true},
        })));
        assert!(has_generated_attribute(
            &json!({"attributes.untrusted": {"generated": "1"}})
        ));
        assert!(!has_generated_attribute(&Value::Null));
    }
}
