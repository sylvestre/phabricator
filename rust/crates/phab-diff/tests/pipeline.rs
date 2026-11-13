//! The hunk pipeline of `DifferentialChangesetParser::process()` against
//! PHP's output for `tests/data/pipeline_input.json`, produced by
//! `tests/data/pipeline.php` in the PHP container.

use phab_diff::intraline::{Segment, SegmentKind};
use phab_diff::parser::{DepthChange, HunkParser, Line, LineType, ParseError};
use phab_diff::realign::{self, DiffCommand};
use phab_diff::{ChangeType, Changeset, FileType, Hunk};
use serde_json::{Value, json};

/// The PHP image runs the `diff` of `BusyBox`; use it too when installed.
fn diff_command() -> DiffCommand {
    let busybox = std::process::Command::new("busybox")
        .arg("diff")
        .arg("--help")
        .output()
        .is_ok();
    if busybox {
        DiffCommand::new("busybox", &["diff"])
    } else {
        DiffCommand::default()
    }
}

fn data(name: &str) -> Value {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hunk(spec: &Value) -> Hunk {
    let int = |key: &str| spec[key].as_i64().unwrap();
    Hunk {
        old_offset: int("old_offset"),
        old_len: int("old_len"),
        new_offset: int("new_offset"),
        new_len: int("new_len"),
        changes: spec["changes"].as_str().unwrap().to_owned(),
    }
}

fn line_json(line: Option<&Line>) -> Value {
    line.map_or(Value::Null, |line| {
        let kind = match line.kind {
            None => Value::Null,
            Some(LineType::Added) => json!("+"),
            Some(LineType::Removed) => json!("-"),
            Some(LineType::NoNewline) => json!("\\"),
        };
        json!({"type": kind, "text": line.text, "line": line.line})
    })
}

fn segments_json(segments: &[Segment]) -> Value {
    segments
        .iter()
        .map(|segment| {
            let kind = match segment.kind {
                SegmentKind::Same => json!(0),
                SegmentKind::Changed => json!(1),
                SegmentKind::DepthIn => json!(">"),
                SegmentKind::DepthOut => json!("<"),
            };
            json!([kind, segment.length])
        })
        .collect()
}

fn run(case: &Value) -> Value {
    let hunks: Vec<Hunk> = case["hunks"].as_array().unwrap().iter().map(hunk).collect();
    let context = usize::try_from(case["context"].as_u64().unwrap()).unwrap();

    let mut parser = HunkParser::parse(&hunks).unwrap();
    let changeset = Changeset {
        id: 1,
        phid: None,
        diff_id: 1,
        old_file: None,
        filename: "file".to_owned(),
        away_paths: Vec::new(),
        change_type: ChangeType::Change,
        file_type: FileType::Text,
        metadata: Value::Null,
        old_properties: Value::Null,
        new_properties: Value::Null,
        add_lines: 0,
        del_lines: 0,
        hunks: hunks.clone(),
    };
    realign::realign(&changeset, &mut parser, &diff_command()).unwrap();
    parser.reparse_for_special_attributes();
    let intraline = parser.intraline_diffs();

    let lines_mask: Vec<usize> = parser
        .visible_lines_mask(context)
        .iter()
        .enumerate()
        .filter_map(|(row, &shown)| shown.then_some(row))
        .collect();
    json!({
        "name": case["name"],
        "old": parser.old_lines.iter().map(Option::as_ref).map(line_json).collect::<Vec<_>>(),
        "new": parser.new_lines.iter().map(Option::as_ref).map(line_json).collect::<Vec<_>>(),
        "has_any_changes": parser.has_any_changes(),
        "is_deleted": parser.is_deleted(),
        "intraline": intraline
            .diffs
            .iter()
            .map(|(row, diff)| json!([row, segments_json(&diff.old), segments_json(&diff.new)]))
            .collect::<Vec<_>>(),
        "depth_only": intraline
            .depth_only
            .iter()
            .map(|(row, change)| json!([row, match change {
                DepthChange::In => ">",
                DepthChange::Out => "<",
            }]))
            .collect::<Vec<_>>(),
        "lines_mask": lines_mask,
        "blocks_mask": parser.visible_blocks_mask(context),
        "old_corpus": parser.old_corpus(),
        "new_corpus": parser.new_corpus(),
        "hunk_starts": HunkParser::hunk_start_lines(&hunks),
    })
}

#[test]
fn pipeline_matches_php() {
    let inputs = data("pipeline_input.json");
    let expected = data("pipeline_expected.json");
    let inputs = inputs.as_array().unwrap();
    let expected = expected.as_array().unwrap();
    assert_eq!(inputs.len(), expected.len());
    for (input, expected) in inputs.iter().zip(expected) {
        assert_eq!(&run(input), expected, "{}", input["name"]);
    }
}

#[test]
fn leading_no_newline_markers_fail_like_php() {
    let hunks = [Hunk {
        old_offset: 1,
        old_len: 1,
        new_offset: 1,
        new_len: 1,
        changes: "\\ No newline at end of file\n a\n".to_owned(),
    }];
    assert_eq!(HunkParser::parse(&hunks), Err(ParseError::LeadingNoNewline));
    let hunks = [Hunk {
        changes: " a\n*b\n".to_owned(),
        ..hunks[0].clone()
    }];
    assert_eq!(
        HunkParser::parse(&hunks),
        Err(ParseError::LeadingCharacter {
            character: '*',
            index: 1
        })
    );
}
