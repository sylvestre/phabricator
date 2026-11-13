//! Building the view of changesets.

use phab_diff::intraline::{Segment, SegmentKind};
use phab_diff::parser::LineType;
use phab_diff::view::{self, Body, Cell, Row, Shield, ViewOptions};
use phab_diff::{ChangeType, Changeset, FileType, Hunk};
use serde_json::{Value, json};

fn changeset(change_type: ChangeType, file_type: FileType, hunks: Vec<Hunk>) -> Changeset {
    Changeset {
        id: 1,
        phid: None,
        diff_id: 1,
        old_file: None,
        filename: "src/main.c".to_owned(),
        away_paths: Vec::new(),
        change_type,
        file_type,
        metadata: Value::Null,
        old_properties: Value::Null,
        new_properties: Value::Null,
        add_lines: 1,
        del_lines: 1,
        hunks,
    }
}

fn hunk(offset: i64, length: i64, changes: &str) -> Hunk {
    Hunk {
        old_offset: offset,
        old_len: length,
        new_offset: offset,
        new_len: length,
        changes: changes.to_owned(),
    }
}

fn body(changeset: &Changeset, options: &ViewOptions) -> Body {
    view::build(changeset, options).unwrap().body
}

fn context(text: &str, line: i64) -> Cell {
    Cell {
        line,
        kind: None,
        text: format!("{text}\n"),
        segments: Vec::new(),
        depth_only: None,
    }
}

#[test]
fn rows_have_gaps_and_missing_context() {
    let context_lines: Vec<String> = (1..=20).map(|line| format!(" l{line}\n")).collect();
    let changes = context_lines.concat() + "-old\n+new\n l22\n";
    let changeset = changeset(
        ChangeType::Change,
        FileType::Text,
        vec![hunk(10, 22, &changes)],
    );
    let options = ViewOptions {
        lines_context: 2,
        ..ViewOptions::default()
    };
    let Body::Rows(rows) = body(&changeset, &options) else {
        panic!("expected rows");
    };
    // Rows 0..18 are hidden, then 2 lines of context, the change and the
    // last line; the hunk does not start at line 1.
    assert_eq!(rows[0], Row::Gap { top: 0, length: 18 });
    assert_eq!(
        rows[1],
        Row::Line {
            row: 18,
            old: Some(context("l19", 28)),
            new: Some(context("l19", 28)),
        }
    );
    let Row::Line { old, new, .. } = &rows[3] else {
        panic!("expected the changed line, got {:?}", rows[3]);
    };
    let (old, new) = (old.as_ref().unwrap(), new.as_ref().unwrap());
    assert_eq!(
        (old.kind, new.kind),
        (Some(LineType::Removed), Some(LineType::Added))
    );
    assert_eq!(
        old.segments,
        [
            Segment::new(SegmentKind::Changed, 3),
            Segment::new(SegmentKind::Same, 1)
        ]
    );
    assert_eq!(rows.last(), Some(&Row::NoContext));
    assert_eq!(rows.len(), 6);
}

#[test]
fn shields_are_chosen_like_php() {
    let options = ViewOptions::default();
    let unchanged = changeset(ChangeType::Change, FileType::Text, vec![hunk(1, 1, " a\n")]);
    assert_eq!(
        body(&unchanged, &options),
        Body::Shielded(Shield::Unchanged {
            empty_file: false,
            has_content: true,
        })
    );

    let empty = changeset(ChangeType::Add, FileType::Text, Vec::new());
    assert_eq!(
        body(&empty, &options),
        Body::Shielded(Shield::Unchanged {
            empty_file: true,
            has_content: false,
        })
    );

    let deleted = changeset(ChangeType::Delete, FileType::Text, vec![hunk(1, 1, "-a\n")]);
    assert_eq!(body(&deleted, &options), Body::Shielded(Shield::Deleted));

    let moved = changeset(
        ChangeType::MoveAway,
        FileType::Text,
        vec![hunk(1, 1, "-a\n+b\n")],
    );
    assert_eq!(body(&moved, &options), Body::Shielded(Shield::MoveAway));

    let generated = changeset(
        ChangeType::Change,
        FileType::Text,
        vec![hunk(1, 1, "-a\n+// @generated\n")],
    );
    assert_eq!(
        body(&generated, &options),
        Body::Shielded(Shield::Generated)
    );

    let mut attribute = changeset(
        ChangeType::Change,
        FileType::Text,
        vec![hunk(1, 1, "-a\n+b\n")],
    );
    attribute.metadata = json!({"attributes.trusted": {"generated": true}});
    assert_eq!(
        body(&attribute, &options),
        Body::Shielded(Shield::Generated)
    );

    let mut large = changeset(
        ChangeType::Change,
        FileType::Text,
        vec![hunk(1, 1, "-a\n+b\n")],
    );
    large.add_lines = 2000;
    large.del_lines = 501;
    assert_eq!(
        body(&large, &options),
        Body::Shielded(Shield::TooLarge { lines: 2501 })
    );

    // Inline comments lift the shield.
    let commented = ViewOptions {
        has_expanded_comments: true,
        ..ViewOptions::default()
    };
    assert!(matches!(body(&deleted, &commented), Body::Rows(_)));
}

#[test]
fn binary_files_have_no_text() {
    let mut image = changeset(
        ChangeType::Change,
        FileType::Image,
        vec![hunk(1, 1, "-a\n+b\n")],
    );
    image.metadata =
        json!({"old:binary-phid": "PHID-FILE-old", "new:binary-phid": "PHID-FILE-new"});
    let view = view::build(&image, &ViewOptions::default()).unwrap();
    assert_eq!(view.body, Body::NoText);
    assert_eq!(view.old_binary_phid.as_deref(), Some("PHID-FILE-old"));
    assert_eq!(view.new_binary_phid.as_deref(), Some("PHID-FILE-new"));
}
