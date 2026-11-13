//! Port of `DifferentialHunkParser`: lines of both sides of a changeset,
//! aligned for display, with their intraline changes and the mask of lines
//! shown around the changes.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::hunk::Hunk;
use crate::intraline::{self, IntralineDiff, Segment, SegmentKind};
use crate::lines;

/// The change marker of a line; context lines have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineType {
    /// `+`.
    Added,
    /// `-`.
    Removed,
    /// `\`: the `\ No newline at end of file` marker.
    NoNewline,
}

impl LineType {
    fn from_marker(marker: u8) -> Option<Self> {
        match marker {
            b'+' => Some(Self::Added),
            b'-' => Some(Self::Removed),
            b'\\' => Some(Self::NoNewline),
            _ => None,
        }
    }
}

/// One line of one side, the `array('type', 'text', 'line')` of PHP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The change marker; `None` for context.
    pub kind: Option<LineType>,
    /// The text without the marker, with its line ending.
    pub text: String,
    /// The line number on this side.
    pub line: i64,
}

/// Why hunks could not be parsed; PHP throws on these.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// A line starting with something else than ` `, `+`, `-` or `\`.
    #[error("unexpected leading character {character:?} at line index {index}")]
    LeadingCharacter {
        /// The character.
        character: char,
        /// The line index within its hunk.
        index: usize,
    },
    /// A hunk starting with `\ No newline at end of file`: PHP reads the
    /// type of line -1, which its error handler turns into an exception.
    #[error("a hunk starts with a no-newline marker")]
    LeadingNoNewline,
}

/// How the indentation of a line changed, when nothing else did
/// (`DifferentialHunkParser::getDepthOnlyLines()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthChange {
    /// More indented (`'>'`).
    In,
    /// Less indented (`'<'`).
    Out,
}

/// The state of `DifferentialHunkParser` after `parseHunksForLineData()`.
/// `None` entries are the padding `reparseHunksForSpecialAttributes()`
/// adds so both sides have the same length.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HunkParser {
    /// `getOldLines()`.
    pub old_lines: Vec<Option<Line>>,
    /// `getNewLines()`.
    pub new_lines: Vec<Option<Line>>,
    /// `getNormalized()`: the line types come from a whitespace-insensitive
    /// diff of the whole files.
    pub normalized: bool,
}

/// What `generateIntraLineDiffs()` computes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Intraline {
    /// `getIntraLineDiffs()`, by row.
    pub diffs: BTreeMap<usize, IntralineDiff>,
    /// `getDepthOnlyLines()`, by row.
    pub depth_only: BTreeMap<usize, DepthChange>,
}

const TAB_WIDTH: usize = 2;

impl HunkParser {
    /// `parseHunksForLineData()`.
    pub fn parse(hunks: &[Hunk]) -> Result<Self, ParseError> {
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        for hunk in hunks {
            let split = hunk.split_lines();
            let mut types = Vec::with_capacity(split.len());
            let mut texts = Vec::with_capacity(split.len());
            for (index, line) in split.iter().enumerate() {
                match line.as_bytes().first() {
                    None => {
                        types.push(None);
                        texts.push(String::new());
                    }
                    Some(b' ') => {
                        types.push(None);
                        texts.push(line[1..].to_owned());
                    }
                    // A context line whose space was stripped, for example by
                    // email: treat the missing space as still there.
                    Some(b'\r' | b'\n') => {
                        types.push(None);
                        texts.push((*line).to_owned());
                    }
                    Some(&marker @ (b'+' | b'-' | b'\\')) => {
                        types.push(LineType::from_marker(marker));
                        texts.push(line[1..].to_owned());
                    }
                    Some(_) => {
                        return Err(ParseError::LeadingCharacter {
                            character: line.chars().next().unwrap_or_default(),
                            index,
                        });
                    }
                }
            }

            let mut old_line = hunk.old_offset;
            let mut new_line = hunk.new_offset;
            for (cursor, text) in texts.into_iter().enumerate() {
                let mut kind = types[cursor];
                let mut data = Line {
                    kind,
                    text,
                    line: new_line,
                };
                if kind == Some(LineType::NoNewline) {
                    let previous = cursor.checked_sub(1).ok_or(ParseError::LeadingNoNewline)?;
                    kind = types[previous];
                    let trimmed = data.text.len() - lines::php_ltrim(&data.text).len();
                    data.text.drain(..trimmed);
                }
                match kind {
                    Some(LineType::Added) => {
                        new_lines.push(Some(data));
                        new_line += 1;
                    }
                    Some(LineType::Removed) => {
                        data.line = old_line;
                        old_lines.push(Some(data));
                        old_line += 1;
                    }
                    _ => {
                        new_lines.push(Some(data.clone()));
                        data.line = old_line;
                        old_lines.push(Some(data));
                        new_line += 1;
                        old_line += 1;
                    }
                }
            }
        }
        Ok(Self {
            old_lines,
            new_lines,
            normalized: false,
        })
    }

    /// `getHunkStartLines()`: the old lines other than 1 where a hunk
    /// starts, where "Context not available" is shown.
    pub fn hunk_start_lines(hunks: &[Hunk]) -> BTreeSet<i64> {
        hunks
            .iter()
            .map(|hunk| hunk.old_offset)
            .filter(|&line| line > 1)
            .collect()
    }

    fn type_map(lines: &[Option<Line>]) -> BTreeMap<i64, Option<LineType>> {
        lines
            .iter()
            .flatten()
            .map(|line| (line.line, line.kind))
            .collect()
    }

    fn apply_type_map(lines: &mut [Option<Line>], map: &BTreeMap<i64, Option<LineType>>) {
        for line in lines.iter_mut().flatten() {
            line.kind = map.get(&line.line).copied().flatten();
        }
    }

    /// `getOldLineTypeMap()`.
    pub fn old_line_type_map(&self) -> BTreeMap<i64, Option<LineType>> {
        Self::type_map(&self.old_lines)
    }

    /// `getNewLineTypeMap()`.
    pub fn new_line_type_map(&self) -> BTreeMap<i64, Option<LineType>> {
        Self::type_map(&self.new_lines)
    }

    /// `setOldLineTypeMap()`.
    pub fn set_old_line_type_map(&mut self, map: &BTreeMap<i64, Option<LineType>>) {
        Self::apply_type_map(&mut self.old_lines, map);
    }

    /// `setNewLineTypeMap()`.
    pub fn set_new_line_type_map(&mut self, map: &BTreeMap<i64, Option<LineType>>) {
        Self::apply_type_map(&mut self.new_lines, map);
    }

    /// `getIsDeleted()`: there are old lines but no new ones.
    pub fn is_deleted(&self) -> bool {
        !self.new_lines.iter().any(Option::is_some) && self.old_lines.iter().any(Option::is_some)
    }

    /// `getHasAnyChanges()`: whether anything changed, whitespace included.
    pub fn has_any_changes(&self) -> bool {
        self.old_lines.iter().enumerate().any(|(key, old)| {
            let new = self.new_lines.get(key).and_then(Option::as_ref);
            match (old, new) {
                (Some(old), Some(new)) => old.kind != new.kind || old.text != new.text,
                _ => true,
            }
        })
    }

    /// `reparseHunksForSpecialAttributes()`: pads both sides with `None` so
    /// that row `k` of each side is displayed side by side.
    pub fn reparse_for_special_attributes(&mut self) {
        let mut old_stack: Vec<Option<Line>> = self.old_lines.drain(..).rev().collect();
        let mut new_stack: Vec<Option<Line>> = self.new_lines.drain(..).rev().collect();
        let mut rebuild_old = Vec::new();
        let mut rebuild_new = Vec::new();

        while !old_stack.is_empty() || !new_stack.is_empty() {
            let old = old_stack.pop().flatten();
            let new = new_stack.pop().flatten();
            let old_type = old.as_ref().and_then(|line| line.kind);
            let new_type = new.as_ref().and_then(|line| line.kind);

            // This line does not exist in the new file.
            if old_type.is_some() && new_type.is_none() {
                rebuild_old.push(old);
                rebuild_new.push(None);
                if new.is_some() {
                    new_stack.push(new);
                }
                continue;
            }
            // This line does not exist in the old file.
            if new_type.is_some() && old_type.is_none() {
                rebuild_old.push(None);
                rebuild_new.push(new);
                if old.is_some() {
                    old_stack.push(old);
                }
                continue;
            }
            rebuild_old.push(old);
            rebuild_new.push(new);
        }

        self.old_lines = rebuild_old;
        self.new_lines = rebuild_new;
        self.update_change_types_for_normalization();
    }

    /// `updateChangeTypesForNormalization()`: lines aligned by the
    /// whitespace-insensitive diff, but which do differ, are changes.
    fn update_change_types_for_normalization(&mut self) {
        if !self.normalized {
            return;
        }
        for (old, new) in self.old_lines.iter_mut().zip(self.new_lines.iter_mut()) {
            if let (Some(old), Some(new)) = (old, new)
                && old.kind.is_none()
                && new.kind.is_none()
                && old.text != new.text
            {
                old.kind = Some(LineType::Removed);
                new.kind = Some(LineType::Added);
            }
        }
    }

    /// `generateIntraLineDiffs()`.
    pub fn intraline_diffs(&self) -> Intraline {
        let mut result = Intraline::default();
        for (key, old) in self.old_lines.iter().enumerate() {
            let Some(old) = old else {
                continue;
            };
            let Some(Some(new)) = self.new_lines.get(key) else {
                continue;
            };
            if old.kind == new.kind {
                continue;
            }

            let mut old_segments = Vec::new();
            let mut new_segments = Vec::new();
            let mut old_text = old.text.as_str();
            let mut new_text = new.text.as_str();

            if old_text != new_text && lines::php_ltrim(old_text) == lines::php_ltrim(new_text) {
                let old_depth = indent_depth(old_text);
                let new_depth = indent_depth(new_text);
                let mut segment_type = None;
                if old_depth < new_depth {
                    segment_type = Some(DepthChange::In);
                    let width = visual_whitespace_width(new_text, new_depth - old_depth);
                    if width != 0 {
                        new_text = &new_text[width..];
                        new_segments.push(Segment::new(SegmentKind::DepthIn, width));
                    }
                } else if old_depth > new_depth {
                    segment_type = Some(DepthChange::Out);
                    let width = visual_whitespace_width(old_text, old_depth - new_depth);
                    if width != 0 {
                        old_text = &old_text[width..];
                        old_segments.push(Segment::new(SegmentKind::DepthOut, width));
                    }
                }
                // Only the indentation changed.
                if old_text == new_text
                    && let Some(segment_type) = segment_type
                {
                    result.depth_only.insert(key, segment_type);
                }
            }

            let segments = intraline::generate_intraline_diff(old_text, new_text);
            old_segments.extend(segments.old);
            new_segments.extend(segments.new);
            result.diffs.insert(
                key,
                IntralineDiff {
                    old: old_segments,
                    new: new_segments,
                },
            );
        }
        result
    }

    fn is_visible(lines: &[Option<Line>], index: usize) -> bool {
        lines
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|line| line.kind.is_some())
    }

    fn row_count(&self) -> usize {
        self.old_lines.len().max(self.new_lines.len())
    }

    /// `generateVisibleLinesMask()`: which rows are shown with
    /// `lines_context` lines of context around changes. As in PHP, row 0 is
    /// never part of the mask.
    pub fn visible_lines_mask(&self, lines_context: usize) -> Vec<bool> {
        let length = self.row_count();
        let context = i64::try_from(lines_context).unwrap_or(i64::MAX);
        let mut mask = vec![false; length];
        let mut visible = false;
        let mut last: i64 = 0;
        let end = i64::try_from(length).unwrap_or(i64::MAX);
        for cursor in -context..end {
            let offset = cursor + context;
            let visible_at = usize::try_from(offset).is_ok_and(|offset| {
                Self::is_visible(&self.old_lines, offset)
                    || Self::is_visible(&self.new_lines, offset)
            });
            if visible_at {
                visible = true;
                last = offset;
            } else if cursor > last + context {
                visible = false;
            }
            if visible && cursor > 0 {
                mask[usize::try_from(cursor).expect("cursor is positive")] = true;
            }
        }
        mask
    }

    /// `generateVisibleBlocksMask()` (T13468): rows within `lines_context`
    /// of a change.
    pub fn visible_blocks_mask(&self, lines_context: usize) -> Vec<bool> {
        let length = self.row_count();
        let mut mask = vec![false; length];
        let mut reveal_cursor = 0;
        for row in 0..length {
            if !(Self::is_visible(&self.old_lines, row) || Self::is_visible(&self.new_lines, row)) {
                continue;
            }
            let min_reveal = row.saturating_sub(lines_context).max(reveal_cursor);
            let max_reveal = (row + lines_context).min(length - 1);
            for revealed in mask.iter_mut().take(max_reveal + 1).skip(min_reveal) {
                *revealed = true;
            }
            reveal_cursor = max_reveal + 1;
        }
        mask
    }

    fn corpus(lines: &[Option<Line>]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|line| match line {
                None => Some("\n".to_owned()),
                Some(line) if line.kind == Some(LineType::NoNewline) => None,
                Some(line) => Some(line.text.clone()),
            })
            .collect()
    }

    /// `getOldCorpus()`: the old text to highlight, a newline per padding row.
    pub fn old_corpus(&self) -> Vec<String> {
        Self::corpus(&self.old_lines)
    }

    /// `getNewCorpus()`.
    pub fn new_corpus(&self) -> Vec<String> {
        Self::corpus(&self.new_lines)
    }
}

/// `getIndentDepth()`: the visual width of the leading spaces and tabs.
fn indent_depth(text: &str) -> usize {
    let mut depth = 0;
    for byte in text.bytes() {
        match byte {
            b' ' => depth += 1,
            b'\t' => {
                depth += TAB_WIDTH;
                depth -= depth % TAB_WIDTH;
            }
            _ => break,
        }
    }
    depth
}

/// `getCharacterCountForVisualWhitespace()`: the longest whitespace prefix
/// fitting in `depth` columns, in bytes.
fn visual_whitespace_width(text: &str, depth: usize) -> usize {
    let mut characters = 0;
    let mut visual = 0;
    for byte in text.bytes() {
        if visual >= depth {
            break;
        }
        match byte {
            b' ' => {
                characters += 1;
                visual += 1;
            }
            b'\t' => {
                let next_stop = visual + TAB_WIDTH - (visual + TAB_WIDTH) % TAB_WIDTH;
                let tab_visual = next_stop - visual;
                if depth - visual < tab_visual {
                    break;
                }
                characters += 1;
                visual += tab_visual;
            }
            _ => break,
        }
    }
    characters
}

/// Which side a context diff is about (`makeContextDiff()`'s `$is_new`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The old file.
    Old,
    /// The new file.
    New,
}

/// `DifferentialHunkParser::makeContextDiff()`: the part of the hunks
/// around `line_number` .. `line_number + line_length` on `side`, with
/// `add_context` more lines, as shown next to inline comments in mail.
pub fn make_context_diff(
    hunks: &[Hunk],
    side: Side,
    line_number: i64,
    line_length: i64,
    add_context: i64,
) -> String {
    let prefix = match side {
        Side::Old => b'-',
        Side::New => b'+',
    };
    let mut output = Vec::new();
    for hunk in hunks {
        let (offset, length) = match side {
            Side::Old => (hunk.old_offset, hunk.old_len),
            Side::New => (hunk.new_offset, hunk.new_len),
        };
        let start = line_number - offset;
        let end = start + line_length;
        // Go in when start == length too: the last line may be a "\ No
        // newline at end of file" marker, shown with extra context.
        if start > length || end < 0 {
            continue;
        }
        let start = start - add_context;
        let end = end + add_context;

        let mut content = Vec::new();
        let (mut old_pos, mut new_pos) = (0i64, 0i64);
        let (mut old_first, mut new_first) = (None, None);
        let (mut old_last, mut new_last) = (0i64, 0i64);
        for line in hunk.changes.split('\n') {
            let first = line.as_bytes().first().copied();
            let in_common = first == Some(b' ');
            let in_old = first == Some(b'-') || in_common;
            let in_new = first == Some(b'+') || in_common;
            let in_selected = first == Some(prefix);
            let skip = !in_selected && !in_common;
            let position = if prefix == b'-' { old_pos } else { new_pos };
            if position <= end {
                if start <= position && (!skip || (position != start && position != end)) {
                    if in_old {
                        old_first.get_or_insert(old_pos);
                        old_last = old_pos;
                    }
                    if in_new {
                        new_first.get_or_insert(new_pos);
                        new_last = new_pos;
                    }
                    content.push(line);
                }
                if in_old {
                    old_pos += 1;
                }
                if in_new {
                    new_pos += 1;
                }
            }
        }
        if old_first.is_some() || new_first.is_some() {
            let old_range = old_first.map_or_else(String::new, |first| {
                format!(" -{},{}", hunk.old_offset + first, old_last - first + 1)
            });
            let new_range = new_first.map_or_else(String::new, |first| {
                format!(" +{},{}", hunk.new_offset + first, new_last - first + 1)
            });
            output.push(format!("@@{old_range}{new_range} @@"));
            output.push(content.join("\n"));
        }
    }
    output.join("\n")
}
