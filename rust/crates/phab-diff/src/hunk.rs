//! A hunk of a changeset and the file contents it holds
//! (`DifferentialHunk`, without its storage).

use crate::lines;

/// One hunk: where it starts on each side and its unified-diff body, each
/// line starting with ` `, `+`, `-` or `\`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// `oldOffset`: the first line on the old side.
    pub old_offset: i64,
    /// `oldLen`: the number of lines on the old side.
    pub old_len: i64,
    /// `newOffset`: the first line on the new side.
    pub new_offset: i64,
    /// `newLen`: the number of lines on the new side.
    pub new_len: i64,
    /// `DifferentialHunk::getChanges()`: the body, decoded to UTF-8.
    pub changes: String,
}

/// Which lines [`Hunk::content`] keeps (`DifferentialHunk::makeContent()`'s
/// `$include`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Include {
    /// Lines starting with `+`.
    pub added: bool,
    /// Lines starting with `-`.
    pub removed: bool,
    /// Lines starting with a space.
    pub context: bool,
}

impl Include {
    /// The old file: context and removed lines (`" -"`).
    pub const OLD: Self = Self {
        added: false,
        removed: true,
        context: true,
    };
    /// The new file: context and added lines (`" +"`).
    pub const NEW: Self = Self {
        added: true,
        removed: false,
        context: true,
    };

    fn includes(self, marker: u8) -> bool {
        match marker {
            b'+' => self.added,
            b'-' => self.removed,
            b' ' => self.context,
            _ => false,
        }
    }
}

impl Hunk {
    /// `DifferentialHunk::getSplitLines()`.
    pub fn split_lines(&self) -> Vec<&str> {
        lines::split_lines(&self.changes)
    }

    /// `DifferentialHunk::makeContent()`: the kept lines without their
    /// marker, keyed by line number. A `\ No newline at end of file` marker
    /// strips the newline of the kept line before it.
    pub fn content(&self, include: Include) -> Vec<(i64, String)> {
        let mut number = if include.added {
            self.new_offset
        } else {
            self.old_offset
        };
        let mut results: Vec<(i64, String)> = Vec::new();
        let mut use_next_newline = false;
        for line in self.split_lines() {
            let Some(&marker) = line.as_bytes().first() else {
                continue;
            };
            if marker == b'\\' {
                if use_next_newline && let Some((_, text)) = results.last_mut() {
                    let trimmed = text.trim_end_matches('\n').len();
                    text.truncate(trimmed);
                }
            } else if include.includes(marker) {
                use_next_newline = true;
                results.push((number, line[1..].to_owned()));
            } else {
                use_next_newline = false;
            }
            if marker == b' ' || include.includes(marker) {
                number += 1;
            }
        }
        results
    }

    /// `DifferentialHunk::makeOldFile()`.
    pub fn make_old_file(&self) -> String {
        concat(self.content(Include::OLD))
    }

    /// `DifferentialHunk::makeNewFile()`.
    pub fn make_new_file(&self) -> String {
        concat(self.content(Include::NEW))
    }
}

fn concat(lines: Vec<(i64, String)>) -> String {
    lines.into_iter().map(|(_, text)| text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(changes: &str) -> Hunk {
        Hunk {
            old_offset: 1,
            old_len: 0,
            new_offset: 1,
            new_len: 0,
            changes: changes.to_owned(),
        }
    }

    #[test]
    fn files_are_rebuilt() {
        let hunk = hunk(" a\n-b\n+c\n d\n");
        assert_eq!(hunk.make_old_file(), "a\nb\nd\n");
        assert_eq!(hunk.make_new_file(), "a\nc\nd\n");
        assert_eq!(
            hunk.content(Include::NEW),
            [
                (1, "a\n".to_owned()),
                (2, "c\n".to_owned()),
                (3, "d\n".to_owned())
            ]
        );
    }

    #[test]
    fn missing_newlines_apply_to_their_side() {
        let hunk = hunk("-x\n\\ No newline at end of file\n+x\n");
        assert_eq!(hunk.make_old_file(), "x");
        assert_eq!(hunk.make_new_file(), "x\n");
        let hunk = self::hunk("-x\n+x\n\\ No newline at end of file\n");
        assert_eq!(hunk.make_old_file(), "x\n");
        assert_eq!(hunk.make_new_file(), "x");
    }
}
