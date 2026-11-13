//! `DifferentialHunkParserTestCase`, ported.

use std::collections::BTreeSet;

use phab_diff::parser::{self, HunkParser, Side};
use phab_diff::{Hunk, unified};
use rstest::rstest;

fn hunk(old_offset: i64, old_len: i64, new_offset: i64, new_len: i64, changes: &str) -> Hunk {
    Hunk {
        old_offset,
        old_len,
        new_offset,
        new_len,
        changes: changes.to_owned(),
    }
}

/// `createSingleChange()`: one hunk starting at line 1.
fn single(old_len: i64, new_len: i64, changes: &str) -> Vec<Hunk> {
    vec![hunk(1, old_len, 1, new_len, changes)]
}

const MULTI: &str = " e1\n e2\n-o3\n-o4\n+n3\n e5/4\n e6/5\n+n6\n e7\n";

#[rstest]
#[case::one_line_old_comment(single(1, 0, "-a"), Side::Old, 1, 0, 0, "@@ -1,1 @@\n-a")]
#[case::one_line_new_comment(single(0, 1, "+a"), Side::New, 1, 0, 0, "@@ +1,1 @@\n+a")]
#[case::cannot_find_context(single(0, 1, "+a"), Side::New, 2, 0, 0, "")]
#[case::overlap_from_start_of_hunk(
    vec![hunk(23, 2, 42, 2, " 1\n 2")], Side::New, 41, 1, 0, "@@ -23,1 +42,1 @@\n 1"
)]
#[case::overlap_after_end_of_hunk(
    vec![hunk(23, 2, 42, 2, " 1\n 2")], Side::New, 43, 1, 0, "@@ -24,1 +43,1 @@\n 2"
)]
#[case::new_file_in_old_comment_from_start(
    single(2, 3, "+n1\n e1/2\n-o2\n+n3\n"),
    Side::Old,
    1,
    1,
    0,
    "@@ -1,2 +2,1 @@\n e1/2\n-o2"
)]
#[case::old_file_in_new_comment_from_start(
    single(2, 2, "-o1\n e2/1\n-o3\n+n2\n"),
    Side::New,
    1,
    1,
    0,
    "@@ -2,1 +1,2 @@\n e2/1\n+n2"
)]
#[case::no_newline_at_end_of_file(
    single(0, 1, "+a\n\\No newline at end of file"),
    Side::New,
    2,
    0,
    1,
    "@@ +1,1 @@\n+a\n\\No newline at end of file"
)]
#[case::multi_line_new_comment(
    single(7, 7, MULTI),
    Side::New,
    2,
    4,
    0,
    "@@ -2,5 +2,5 @@\n e2\n-o3\n-o4\n+n3\n e5/4\n e6/5\n+n6"
)]
#[case::multi_line_old_comment(
    single(7, 7, MULTI),
    Side::Old,
    2,
    4,
    0,
    "@@ -2,5 +2,4 @@\n e2\n-o3\n-o4\n+n3\n e5/4\n e6/5"
)]
#[case::new_file_in_old_comment_from_start_with_context(
    single(2, 3, "+n1\n e1/2\n-o2\n+n3\n"),
    Side::Old,
    1,
    1,
    1,
    "@@ -1,2 +1,2 @@\n+n1\n e1/2\n-o2"
)]
#[case::old_file_in_new_comment_from_start_with_context(
    single(2, 2, "-o1\n e2/1\n-o3\n+n2\n"),
    Side::New,
    1,
    1,
    1,
    "@@ -1,3 +1,2 @@\n-o1\n e2/1\n-o3\n+n2"
)]
fn context_diffs(
    #[case] hunks: Vec<Hunk>,
    #[case] side: Side,
    #[case] line_number: i64,
    #[case] line_length: i64,
    #[case] add_context: i64,
    #[case] expected: &str,
) {
    assert_eq!(
        parser::make_context_diff(&hunks, side, line_number, line_length, add_context),
        expected
    );
}

#[rstest]
#[case("missing_context.diff", &[4])]
#[case("missing_context_2.diff", &[5])]
#[case("missing_context_3.diff", &[4, 13])]
fn missing_context(#[case] name: &str, #[case] expected: &[i64]) {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    let diff = std::fs::read_to_string(path).unwrap();
    let hunks = unified::parse_hunks(&diff).unwrap();
    assert_eq!(
        HunkParser::hunk_start_lines(&hunks),
        expected.iter().copied().collect::<BTreeSet<_>>()
    );
}
