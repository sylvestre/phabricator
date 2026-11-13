//! Changes within a line: `ArcanistDiffUtils::generateIntralineDiff()`.

use crate::edit_distance::{Costs, EditDistance, Smoothing};

/// What a [`Segment`] of a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Unchanged text (`0`).
    Same,
    /// Changed text (`1`).
    Changed,
    /// Indentation added on the new side (`'>'`).
    DepthIn,
    /// Indentation removed on the old side (`'<'`).
    DepthOut,
}

/// A run of bytes of a line, as the `array($kind, $length)` pairs of PHP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// What the run is.
    pub kind: SegmentKind,
    /// Its length in bytes.
    pub length: usize,
}

impl Segment {
    /// A segment of `kind` and `length` bytes.
    pub fn new(kind: SegmentKind, length: usize) -> Self {
        Self { kind, length }
    }
}

/// The segments of the old and the new line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntralineDiff {
    /// Segments of the old line.
    pub old: Vec<Segment>,
    /// Segments of the new line.
    pub new: Vec<Segment>,
}

/// Lines longer than this many characters are not compared.
const MAX_GLYPHS: usize = 100;

/// `ArcanistDiffUtils::generateIntralineDiff()`.
pub fn generate_intraline_diff(old: &str, new: &str) -> IntralineDiff {
    let (old_length, new_length) = (old.len(), new.len());
    if old == new || old.is_empty() || new.is_empty() {
        return IntralineDiff {
            old: vec![Segment::new(SegmentKind::Same, old_length)],
            new: vec![Segment::new(SegmentKind::Same, new_length)],
        };
    }
    // A quick check on bytes before splitting into characters (T11744).
    if old_length + new_length > 16 * MAX_GLYPHS {
        return IntralineDiff {
            old: vec![Segment::new(SegmentKind::Changed, old_length)],
            new: vec![Segment::new(SegmentKind::Changed, new_length)],
        };
    }
    compute_intraline_edits(old, new)
}

/// `ArcanistDiffUtils::generateEditString()`.
pub fn generate_edit_string<T: PartialEq>(old: &[T], new: &[T], maximum: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let alter = 1.0 / (maximum as f64 * 2.0);
    let costs = Costs {
        alter,
        replace: 2.0,
        maximum_length: Some(maximum),
        smoothing: Smoothing::Internal,
        ..Costs::default()
    };
    EditDistance::new(costs, old, new).edit_string()
}

/// `phutil_utf8_is_combining_character()` for one character.
fn is_combining(character: char) -> bool {
    matches!(
        u32::from(character),
        0x0300..=0x036F | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

/// `phutil_utf8v_combined()`: characters with the combining characters
/// that follow them. A leading combining character gets a space in front,
/// which PHP then counts as a byte of the line.
pub fn utf8v_combined(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut buffer: Option<String> = None;
    for character in text.chars() {
        if is_combining(character) {
            buffer.get_or_insert_with(|| " ".to_owned()).push(character);
        } else {
            if let Some(previous) = buffer.take() {
                parts.push(previous);
            }
            buffer = Some(character.to_string());
        }
    }
    parts.extend(buffer);
    parts
}

/// `ArcanistDiffUtils::computeIntralineEdits()`: the edit string on
/// characters, turned into byte runs.
fn compute_intraline_edits(old: &str, new: &str) -> IntralineDiff {
    let multibyte = !(old.is_ascii() && new.is_ascii());
    let (old_chars, new_chars): (Vec<String>, Vec<String>) = if multibyte {
        (utf8v_combined(old), utf8v_combined(new))
    } else {
        (
            old.chars().map(String::from).collect(),
            new.chars().map(String::from).collect(),
        )
    };
    let edits = generate_edit_string(&old_chars, &new_chars, MAX_GLYPHS);

    let (mut old_position, mut new_position) = (0, 0);
    let (mut old_runs, mut new_runs) = (Vec::new(), Vec::new());
    // PHP reads past the end of a side as an empty character.
    let length_at = |chars: &[String], position: usize| chars.get(position).map_or(0, String::len);
    for operation in edits.bytes() {
        let (old_bytes, new_bytes) = match operation {
            b's' | b'x' => {
                let lengths = (
                    length_at(&old_chars, old_position),
                    length_at(&new_chars, new_position),
                );
                old_position += 1;
                new_position += 1;
                lengths
            }
            b'i' => {
                let length = length_at(&new_chars, new_position);
                new_position += 1;
                (0, length)
            }
            _ => {
                let length = length_at(&old_chars, old_position);
                old_position += 1;
                (length, 0)
            }
        };
        let kind = if operation == b's' {
            SegmentKind::Same
        } else {
            SegmentKind::Changed
        };
        if old_bytes != 0 {
            old_runs.push(Segment::new(kind, old_bytes));
        }
        if new_bytes != 0 {
            new_runs.push(Segment::new(kind, new_bytes));
        }
    }
    IntralineDiff {
        old: collapse_runs(old_runs),
        new: collapse_runs(new_runs),
    }
}

/// `ArcanistDiffUtils::collapseIntralineRuns()`.
fn collapse_runs(runs: Vec<Segment>) -> Vec<Segment> {
    let mut collapsed: Vec<Segment> = Vec::with_capacity(runs.len());
    for run in runs {
        match collapsed.last_mut() {
            Some(last) if last.kind == run.kind => last.length += run.length,
            _ => collapsed.push(run),
        }
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn chars(text: &str) -> Vec<u8> {
        text.bytes().collect()
    }

    // From ArcanistDiffUtilsTestCase::testLevenshtein().
    #[rstest]
    #[case("a", "b", "x")]
    #[case("kalrmr(array($b))", "array($b)", "dddddddssssssssds")]
    #[case("array($b)", "kalrmr(array($b))", "iiiiiiissssssssis")]
    #[case("zkalrmr(array($b))z", "xarray($b)x", "dddddddxsssssssssdx")]
    #[case("xarray($b)x", "zkalrmr(array($b))z", "iiiiiiixsssssssssix")]
    #[case("abcdefghi", "abcdefghi", "sssssssss")]
    #[case("abcdefghi", "abcdefghijkl", "sssssssssiii")]
    #[case("abcdefghijkl", "abcdefghi", "sssssssssddd")]
    #[case("xyzabcdefghi", "abcdefghi", "dddsssssssss")]
    #[case("abcdefghi", "xyzabcdefghi", "iiisssssssss")]
    #[case("abcdefg", "abxdxfg", "ssxxxss")]
    #[case(
        "private function a($a, $b) {",
        "public function and($b, $c) {",
        "siixxdddxsssssssssssiixxxxxxxsss"
    )]
    fn edit_strings(#[case] old: &str, #[case] new: &str, #[case] expected: &str) {
        assert_eq!(generate_edit_string(&chars(old), &chars(new), 80), expected);
        let (old, new) = (utf8v_combined(old), utf8v_combined(new));
        assert_eq!(generate_edit_string(&old, &new, 80), expected);
    }

    #[test]
    fn long_lines_with_small_changes_are_compared() {
        let xs = "x".repeat(168);
        let old = format!("        if ({xs}) {{");
        let new = format!("        if({xs}) {{");
        let expected = format!("ssssssssssds{}", "s".repeat(171));
        assert_eq!(
            generate_edit_string(&chars(&old), &chars(&new), 80),
            expected
        );
    }

    fn runs(pairs: &[(u8, usize)]) -> Vec<Segment> {
        pairs
            .iter()
            .map(|&(kind, length)| {
                let kind = if kind == 0 {
                    SegmentKind::Same
                } else {
                    SegmentKind::Changed
                };
                Segment::new(kind, length)
            })
            .collect()
    }

    const CAT: &str = "Grumpy\u{2603}at";

    // From ArcanistDiffUtilsTestCase::testGenerateUTF8IntralineDiff().
    #[rstest]
    #[case("", "", &[(0, 0)], &[(0, 0)])]
    #[case("", CAT, &[(0, 0)], &[(0, 11)])]
    #[case(CAT, "", &[(0, 11)], &[(0, 0)])]
    #[case(CAT, CAT, &[(0, 11)], &[(0, 11)])]
    #[case(CAT, "Smiling Dog", &[(1, 11)], &[(1, 11)])]
    #[case("GrumpyCat", CAT, &[(0, 6), (1, 1), (0, 2)], &[(0, 6), (1, 3), (0, 2)])]
    #[case(
        "GrumpyCat",
        "Grumpy\u{2603}a\u{2603}t",
        &[(0, 6), (1, 2), (0, 1)],
        &[(0, 6), (1, 7), (0, 1)]
    )]
    #[case("GrumpyC\u{2603}t", "DrumpyC\u{2603}t", &[(1, 1), (0, 10)], &[(1, 1), (0, 10)])]
    #[case("GrumpyC\u{2603}t", "GrumpyC\u{2603}P", &[(0, 10), (1, 1)], &[(0, 10), (1, 1)])]
    #[case(
        "GrumpyC\u{2603}t",
        "DrumpyC\u{2603}P",
        &[(1, 1), (0, 9), (1, 1)],
        &[(1, 1), (0, 9), (1, 1)]
    )]
    #[case("Senor", "Sen\u{0360}or", &[(0, 2), (1, 1), (0, 2)], &[(0, 2), (1, 3), (0, 2)])]
    fn intraline_diffs(
        #[case] old: &str,
        #[case] new: &str,
        #[case] expected_old: &[(u8, usize)],
        #[case] expected_new: &[(u8, usize)],
    ) {
        let diff = generate_intraline_diff(old, new);
        assert_eq!(diff.old, runs(expected_old));
        assert_eq!(diff.new, runs(expected_new));
    }

    #[test]
    fn leading_combining_characters_get_a_space() {
        assert_eq!(
            utf8v_combined("\u{0300}a\u{0301}b"),
            [" \u{0300}", "a\u{0301}", "b"]
        );
        assert_eq!(utf8v_combined(""), Vec::<String>::new());
    }
}
