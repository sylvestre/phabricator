//! Hunks of a one-file unified diff, as `ArcanistDiffParser::parseChangeset()`
//! reads them. Used for the diffs PHP builds itself, such as the
//! whitespace-insensitive diff used for alignment; diffs uploaded by
//! clients are parsed by PHP and stored as hunks.

use thiserror::Error;

use crate::hunk::Hunk;
use crate::lines;

/// Why a diff could not be read (`ArcanistDiffParser::didFailParse()`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UnifiedError {
    /// A line where a `@@ -NN,NN +NN,NN @@` header was expected.
    #[error("expected a hunk header, got {0:?}")]
    ExpectedHeader(String),
    /// A `\` line other than `\ No newline at end of file`.
    #[error("expected \"\\ No newline at end of file\", got {0:?}")]
    ExpectedNoNewline(String),
    /// The body has fewer lines than the header says.
    #[error("found the wrong number of hunk lines")]
    WrongLineCount,
}

/// The offsets and lengths of a `@@ -a,b +c,d @@` header; a missing
/// length means one line.
fn parse_header(line: &str) -> Option<[i64; 4]> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(i64, i64)> {
        let (offset, length) = text.split_once(',').unwrap_or((text, "1"));
        let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        if !digits(offset) || !digits(length) {
            return None;
        }
        Some((offset.parse().ok()?, length.parse().ok()?))
    };
    let (old_offset, old_len) = range(old)?;
    let (new_offset, new_len) = range(new)?;
    Some([old_offset, old_len, new_offset, new_len])
}

/// The hunks of a diff of one file. Lines before the first hunk header
/// (`---`, `+++`, `diff`, `index`) are skipped.
pub fn parse_hunks(diff: &str) -> Result<Vec<Hunk>, UnifiedError> {
    let text: Vec<&str> = lines::split_lines(diff);
    let Some(mut cursor) = text.iter().position(|line| line.starts_with("@@ ")) else {
        return Ok(Vec::new());
    };
    let mut hunks = Vec::new();
    loop {
        let header = text[cursor].trim_matches(['\r', '\n']);
        let [old_offset, mut old_len, new_offset, mut new_len] =
            parse_header(header).ok_or_else(|| UnifiedError::ExpectedHeader(header.to_owned()))?;
        let (old_total, new_total) = (old_len, new_len);
        let mut body = String::new();
        let mut hit_next_hunk = false;
        loop {
            cursor += 1;
            let Some(line) = text.get(cursor) else {
                break;
            };
            // An empty line is an unchanged empty line whose space was
            // dropped (git's diff.suppress-blank-empty).
            let marker = if line.trim_end_matches(['\r', '\n']).is_empty() {
                b' '
            } else {
                line.as_bytes()[0]
            };
            match marker {
                b'\\' => {
                    if !line.contains("\\ No newline at end of file") {
                        return Err(UnifiedError::ExpectedNoNewline((*line).to_owned()));
                    }
                    body.push_str(line);
                    if new_len == 0 {
                        break;
                    }
                }
                b'+' => {
                    new_len -= 1;
                    body.push_str(line);
                }
                b'-' => {
                    if old_len == 0 {
                        // The "---" of the next file.
                        hit_next_hunk = true;
                        break;
                    }
                    old_len -= 1;
                    body.push_str(line);
                }
                b' ' => {
                    if old_len == 0 && new_len == 0 {
                        break;
                    }
                    old_len -= 1;
                    new_len -= 1;
                    body.push_str(line);
                }
                _ => {
                    hit_next_hunk = true;
                    break;
                }
            }
        }
        if old_len != 0 || new_len != 0 {
            return Err(UnifiedError::WrongLineCount);
        }
        hunks.push(Hunk {
            old_offset,
            old_len: old_total,
            new_offset,
            new_len: new_total,
            changes: body,
        });

        if !hit_next_hunk {
            // nextNonemptyLine().
            cursor += 1;
            while text.get(cursor).is_some_and(|line| line.trim().is_empty()) {
                cursor += 1;
            }
        }
        match text.get(cursor) {
            Some(line) if line.starts_with("@@ ") => {}
            _ => break,
        }
    }
    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("@@ -1,2 +3,4 @@", Some([1, 2, 3, 4]))]
    #[case("@@ -1 +3 @@ fn main()", Some([1, 1, 3, 1]))]
    #[case("@@ -0,0 +1,2 @@", Some([0, 0, 1, 2]))]
    #[case("@@ -a,1 +1 @@", None)]
    #[case("@@ -1,2 +3,4", None)]
    fn headers(#[case] line: &str, #[case] expected: Option<[i64; 4]>) {
        assert_eq!(parse_header(line), expected);
    }

    #[test]
    fn hunks_are_read() {
        let diff = "--- a\n+++ b\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n@@ -9 +9,2 @@\n x\n+y\n\\ No newline at end of file\n";
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].changes, " a\n-b\n+c\n");
        assert_eq!(
            (hunks[1].old_offset, hunks[1].old_len, hunks[1].new_len),
            (9, 1, 2)
        );
        assert_eq!(hunks[1].changes, " x\n+y\n\\ No newline at end of file\n");
    }

    #[test]
    fn short_bodies_are_errors() {
        assert_eq!(
            parse_hunks("@@ -1,3 +1,3 @@\n a\n"),
            Err(UnifiedError::WrongLineCount)
        );
    }
}
