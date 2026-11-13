//! Diffs as HTML: a unified view of what `phab_diff::view` built. The text
//! is escaped; intraline changes are highlighted.

use std::fmt::Write;

use phab_diff::ChangeType;
use phab_diff::intraline::SegmentKind;
use phab_diff::parser::LineType;
use phab_diff::view::{Body, Cell, ChangesetView, Row, Shield};

/// `htmlspecialchars()`.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#039;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// What happened to a file, as the header says it.
fn change_label(view: &ChangesetView) -> String {
    let from = view.old_file.as_deref().map(escape).unwrap_or_default();
    match view.change_type {
        ChangeType::Add => "Added".to_owned(),
        ChangeType::Delete => "Deleted".to_owned(),
        ChangeType::MoveHere => format!("Moved from {from}"),
        ChangeType::CopyHere => format!("Copied from {from}"),
        ChangeType::MoveAway => "Moved away".to_owned(),
        ChangeType::CopyAway | ChangeType::Multicopy => "Copied away".to_owned(),
        _ => String::new(),
    }
}

/// The messages of `renderShield()`.
fn shield_message(shield: Shield) -> String {
    match shield {
        Shield::Generated => {
            "This file contains generated code, which does not normally need to be reviewed."
                .to_owned()
        }
        Shield::MoveAway => String::new(),
        Shield::Unchanged {
            empty_file: true, ..
        } => "This is an empty file.".to_owned(),
        Shield::Unchanged { .. } => "The contents of this file were not changed.".to_owned(),
        Shield::Deleted => "This file was completely deleted.".to_owned(),
        Shield::TooLarge { lines } => {
            format!("This file has a very large number of changes ({lines} lines).")
        }
    }
}

/// The text of a cell, without its line ending, with changed runs marked.
fn cell_text(cell: &Cell) -> String {
    let text = cell.text.strip_suffix('\n').unwrap_or(&cell.text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if cell.segments.is_empty() {
        return escape(text);
    }
    let mut html = String::new();
    let mut offset = 0;
    for segment in &cell.segments {
        let end = (offset + segment.length).min(text.len());
        // Segments count bytes; never cut inside a character.
        let piece = text.get(offset..end).unwrap_or_default();
        match segment.kind {
            SegmentKind::Same => html.push_str(&escape(piece)),
            SegmentKind::Changed => {
                let _ = write!(html, "<span class=\"bright\">{}</span>", escape(piece));
            }
            SegmentKind::DepthIn | SegmentKind::DepthOut => {
                let _ = write!(html, "<span class=\"depth\">{}</span>", escape(piece));
            }
        }
        offset = end;
    }
    html.push_str(&escape(text.get(offset..).unwrap_or_default()));
    html
}

fn line_row(
    html: &mut String,
    class: &str,
    old: Option<i64>,
    new: Option<i64>,
    marker: char,
    text: &str,
) {
    let number = |line: Option<i64>| line.map(|line| line.to_string()).unwrap_or_default();
    let _ = writeln!(
        html,
        "<tr class=\"{class}\"><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"code\"><span class=\"marker\">{marker}</span>{text}</td></tr>",
        number(old),
        number(new),
    );
}

fn rows_html(html: &mut String, rows: &[Row], inlines: &[Inline]) {
    let mut shown = vec![false; inlines.len()];
    html.push_str("<table class=\"diff\">\n");
    for row in rows {
        match row {
            Row::Line { old, new, .. } => {
                let here = inlines_at(inlines, &mut shown, old.as_ref(), new.as_ref());
                line_html(html, old.as_ref(), new.as_ref());
                for inline in here {
                    inline_html(html, inline);
                }
            }
            Row::Gap { length, .. } => {
                let _ = writeln!(
                    html,
                    "<tr class=\"diff-gap\"><td colspan=\"3\">{length} lines not shown</td></tr>"
                );
            }
            Row::NoContext => {
                html.push_str(
                    "<tr class=\"diff-gap\"><td colspan=\"3\">Context not available.</td></tr>\n",
                );
            }
        }
    }
    for (inline, shown) in inlines.iter().zip(shown) {
        if !shown {
            inline_html(html, inline);
        }
    }
    html.push_str("</table>\n");
}

/// The rows of one displayed line: context, or the removed then the added
/// side.
fn line_html(html: &mut String, old: Option<&Cell>, new: Option<&Cell>) {
    let changed = |cell: Option<&Cell>| cell.is_some_and(|cell| cell.kind.is_some());
    if !changed(old) && !changed(new) {
        if let Some(cell) = new.or(old) {
            line_row(
                html,
                "diff-context",
                old.map(|cell| cell.line),
                new.map(|cell| cell.line),
                ' ',
                &cell_text(cell),
            );
        }
        return;
    }
    if let Some(cell) = old.filter(|cell| cell.kind.is_some()) {
        let (class, marker) = match cell.kind {
            Some(LineType::NoNewline) => ("diff-no-newline", '\\'),
            _ => ("diff-removed", '-'),
        };
        line_row(html, class, Some(cell.line), None, marker, &cell_text(cell));
    }
    if let Some(cell) = new.filter(|cell| cell.kind.is_some()) {
        let (class, marker) = match cell.kind {
            Some(LineType::NoNewline) => ("diff-no-newline", '\\'),
            _ => ("diff-added", '+'),
        };
        line_row(html, class, None, Some(cell.line), marker, &cell_text(cell));
    }
}

/// An inline comment, ready to show under the line it ends on.
pub struct Inline {
    /// On the new side, or the old one.
    pub is_new_file: bool,
    /// The line it ends on (`lineNumber + lineLength`).
    pub last_line: i64,
    /// The author, as HTML.
    pub author_html: String,
    /// The comment, as HTML.
    pub body_html: String,
}

fn inline_html(html: &mut String, inline: &Inline) {
    let _ = writeln!(
        html,
        "<tr class=\"diff-inline\"><td colspan=\"3\"><div class=\"inline-comment\"><p class=\"inline-author\">{}</p><div class=\"remarkup\">{}</div></div></td></tr>",
        inline.author_html, inline.body_html
    );
}

/// The inlines ending on a displayed line, once each.
fn inlines_at<'a>(
    inlines: &'a [Inline],
    shown: &mut [bool],
    old: Option<&Cell>,
    new: Option<&Cell>,
) -> Vec<&'a Inline> {
    let mut found = Vec::new();
    for (index, inline) in inlines.iter().enumerate() {
        let cell = if inline.is_new_file { new } else { old };
        if !shown[index] && cell.is_some_and(|cell| cell.line == inline.last_line) {
            shown[index] = true;
            found.push(inline);
        }
    }
    found
}

/// One changeset with its inline comments; the ones on lines not shown
/// come at the end.
pub fn changeset_with_inlines(view: &ChangesetView, inlines: &[Inline]) -> String {
    let mut html = String::new();
    let _ = write!(
        html,
        "<section class=\"changeset\"><h3 class=\"changeset-path\">{}",
        escape(&view.display_filename)
    );
    let label = change_label(view);
    if !label.is_empty() {
        let _ = write!(html, " <span class=\"muted\">{label}</span>");
    }
    html.push_str("</h3>\n");
    match &view.body {
        Body::Shielded(shield) => {
            let message = shield_message(*shield);
            if !message.is_empty() {
                let _ = writeln!(
                    html,
                    "<p class=\"changeset-shield\">{}</p>",
                    escape(&message)
                );
            }
        }
        Body::NoText => html
            .push_str("<p class=\"changeset-shield\">This file has no text content to show.</p>\n"),
        Body::Rows(rows) => rows_html(&mut html, rows, inlines),
    }
    if !matches!(view.body, Body::Rows(_)) && !inlines.is_empty() {
        html.push_str("<table class=\"diff\">\n");
        for inline in inlines {
            inline_html(&mut html, inline);
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");
    html
}

#[cfg(test)]
mod tests {
    use phab_diff::intraline::Segment;

    use super::*;

    fn cell(line: i64, kind: Option<LineType>, text: &str, segments: Vec<Segment>) -> Cell {
        Cell {
            line,
            kind,
            text: text.to_owned(),
            segments,
            depth_only: None,
        }
    }

    #[test]
    fn changed_lines_are_split_and_highlighted() {
        let rows = vec![
            Row::Line {
                row: 0,
                old: Some(cell(1, None, "same <a>\n", vec![])),
                new: Some(cell(1, None, "same <a>\n", vec![])),
            },
            Row::Line {
                row: 1,
                old: Some(cell(
                    2,
                    Some(LineType::Removed),
                    "let x = 1;\n",
                    vec![
                        Segment::new(SegmentKind::Same, 8),
                        Segment::new(SegmentKind::Changed, 1),
                    ],
                )),
                new: Some(cell(
                    2,
                    Some(LineType::Added),
                    "let x = 2;\n",
                    vec![
                        Segment::new(SegmentKind::Same, 8),
                        Segment::new(SegmentKind::Changed, 1),
                    ],
                )),
            },
            Row::Gap { top: 2, length: 5 },
        ];
        let mut html = String::new();
        rows_html(&mut html, &rows, &[]);
        assert!(
            html.contains("<td class=\"code\"><span class=\"marker\"> </span>same &lt;a&gt;</td>"),
            "{html}"
        );
        assert!(
            html.contains("<tr class=\"diff-removed\"><td class=\"n\">2</td><td class=\"n\"></td>"),
            "{html}"
        );
        assert!(
            html.contains("let x = <span class=\"bright\">1</span>;"),
            "{html}"
        );
        assert!(
            html.contains("let x = <span class=\"bright\">2</span>;"),
            "{html}"
        );
        assert!(html.contains("5 lines not shown"), "{html}");
    }
}
