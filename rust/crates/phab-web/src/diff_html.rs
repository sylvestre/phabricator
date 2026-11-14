//! Diffs as HTML: a unified view of what `phab_diff::view` built. The text
//! is escaped; intraline changes and syntax are highlighted.

use std::collections::HashMap;
use std::fmt::Write;
use std::ops::Range;

use phab_diff::ChangeType;
use phab_diff::intraline::SegmentKind;
use phab_diff::parser::LineType;
use phab_diff::view::{Body, Cell, ChangesetView, Row, Shield};
use syntect::parsing::SyntaxReference;

use crate::highlight::{self, Token};
use crate::searchfox::Tree;

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

/// The syntax tokens of each line of the two sides, by line number.
#[derive(Default)]
struct Highlighted {
    old: HashMap<i64, Vec<Token>>,
    new: HashMap<i64, Vec<Token>>,
}

impl Highlighted {
    /// Highlights the lines the rows show, each side as one corpus.
    fn new(syntax: Option<&SyntaxReference>, rows: &[Row]) -> Self {
        let Some(syntax) = syntax else {
            return Self::default();
        };
        let mut old: Vec<&Cell> = Vec::new();
        let mut new: Vec<&Cell> = Vec::new();
        for row in rows {
            if let Row::Line {
                old: old_cell,
                new: new_cell,
                ..
            } = row
            {
                old.extend(old_cell.as_ref().filter(|cell| is_text(cell)));
                new.extend(new_cell.as_ref().filter(|cell| is_text(cell)));
            }
        }
        let side = |cells: Vec<&Cell>| -> HashMap<i64, Vec<Token>> {
            let lines: Vec<&str> = cells.iter().map(|cell| cell.text.as_str()).collect();
            highlight::highlight_lines(syntax, &lines)
                .map(|tokens| cells.iter().map(|cell| cell.line).zip(tokens).collect())
                .unwrap_or_default()
        };
        Self {
            old: side(old),
            new: side(new),
        }
    }

    fn tokens(&self, cell: &Cell, is_new: bool) -> &[Token] {
        let side = if is_new { &self.new } else { &self.old };
        side.get(&cell.line).map_or(&[], Vec::as_slice)
    }
}

/// Whether the cell holds a line of the file, not the "no newline" marker.
fn is_text(cell: &Cell) -> bool {
    cell.kind != Some(LineType::NoNewline)
}

/// The end of a run of `text` at `end`, moved forward to a character
/// boundary: segments count bytes and may end inside a character.
fn char_end(text: &str, end: usize) -> usize {
    let mut end = end.min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    end
}

/// `text[range]` with its syntax tokens, escaped.
fn tokens_html(html: &mut String, text: &str, range: Range<usize>, tokens: &[Token]) {
    let mut offset = range.start;
    for token in tokens {
        let start = char_end(text, token.range.start.max(offset));
        let end = char_end(text, token.range.end.min(range.end));
        if start >= end {
            continue;
        }
        html.push_str(&escape(&text[offset..start]));
        let _ = write!(
            html,
            "<span class=\"{}\">{}</span>",
            token.class,
            escape(&text[start..end])
        );
        offset = end;
    }
    if offset < range.end {
        html.push_str(&escape(&text[offset..range.end]));
    }
}

/// The text of a cell, without its line ending, with its syntax and its
/// changed runs marked.
fn cell_text(cell: &Cell, tokens: &[Token]) -> String {
    let text = cell.text.strip_suffix('\n').unwrap_or(&cell.text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut html = String::new();
    let mut offset = 0;
    for segment in &cell.segments {
        let end = char_end(text, offset + segment.length);
        let class = match segment.kind {
            SegmentKind::Same => None,
            SegmentKind::Changed => Some("bright"),
            SegmentKind::DepthIn | SegmentKind::DepthOut => Some("depth"),
        };
        if let Some(class) = class {
            let _ = write!(html, "<span class=\"{class}\">");
        }
        tokens_html(&mut html, text, offset..end, tokens);
        if class.is_some() {
            html.push_str("</span>");
        }
        offset = end;
    }
    tokens_html(&mut html, text, offset..text.len(), tokens);
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
    // The numbers say where an inline comment goes (`phab.js`).
    let number = |side: &str, line: Option<i64>| match line {
        Some(line) => {
            format!("<td class=\"n\" data-side=\"{side}\" data-line=\"{line}\">{line}</td>")
        }
        None => "<td class=\"n\"></td>".to_owned(),
    };
    let _ = writeln!(
        html,
        "<tr class=\"{class}\">{}{}<td class=\"code\"><span class=\"marker\">{marker}</span>{text}</td></tr>",
        number("old", old),
        number("new", new),
    );
}

fn rows_html(
    html: &mut String,
    rows: &[Row],
    inlines: &[Inline],
    syntax: Option<&SyntaxReference>,
) {
    let highlighted = Highlighted::new(syntax, rows);
    let mut shown = vec![false; inlines.len()];
    html.push_str("<table class=\"diff\">\n");
    for row in rows {
        match row {
            Row::Line { old, new, .. } => {
                let here = inlines_at(inlines, &mut shown, old.as_ref(), new.as_ref());
                line_html(html, old.as_ref(), new.as_ref(), &highlighted);
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
fn line_html(html: &mut String, old: Option<&Cell>, new: Option<&Cell>, highlighted: &Highlighted) {
    let changed = |cell: Option<&Cell>| cell.is_some_and(|cell| cell.kind.is_some());
    if !changed(old) && !changed(new) {
        let shown = new
            .map(|cell| (cell, true))
            .or(old.map(|cell| (cell, false)));
        if let Some((cell, is_new)) = shown {
            line_row(
                html,
                "diff-context",
                old.map(|cell| cell.line),
                new.map(|cell| cell.line),
                ' ',
                &cell_text(cell, highlighted.tokens(cell, is_new)),
            );
        }
        return;
    }
    if let Some(cell) = old.filter(|cell| cell.kind.is_some()) {
        let (class, marker) = match cell.kind {
            Some(LineType::NoNewline) => ("diff-no-newline", '\\'),
            _ => ("diff-removed", '-'),
        };
        let text = cell_text(cell, highlighted.tokens(cell, false));
        line_row(html, class, Some(cell.line), None, marker, &text);
    }
    if let Some(cell) = new.filter(|cell| cell.kind.is_some()) {
        let (class, marker) = match cell.kind {
            Some(LineType::NoNewline) => ("diff-no-newline", '\\'),
            _ => ("diff-added", '+'),
        };
        let text = cell_text(cell, highlighted.tokens(cell, true));
        line_row(html, class, None, Some(cell.line), marker, &text);
    }
}

/// An inline comment, ready to show under the line it ends on.
pub struct Inline {
    /// The comment's PHID, to reply to it.
    pub phid: String,
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
        "<tr class=\"diff-inline\"><td colspan=\"3\"><div class=\"inline-comment\" data-phid=\"{}\"><p class=\"inline-author\">{}</p><div class=\"remarkup\">{}</div></div></td></tr>",
        escape(&inline.phid),
        inline.author_html,
        inline.body_html
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

/// The path of the file before the change, which Searchfox has: none for
/// an added file.
fn path_before(view: &ChangesetView) -> Option<&str> {
    match view.change_type {
        ChangeType::Add => None,
        ChangeType::MoveHere | ChangeType::CopyHere => view.old_file.as_deref(),
        _ => Some(&view.display_filename),
    }
}

/// One changeset with its inline comments; the ones on lines not shown
/// come at the end. With a Searchfox tree, the header links to the file.
pub fn changeset_with_inlines(
    view: &ChangesetView,
    inlines: &[Inline],
    searchfox: Option<&Tree>,
) -> String {
    let mut html = String::new();
    let _ = write!(
        html,
        "<section class=\"changeset\" data-path=\"{0}\"><h3 class=\"changeset-path\">{0}",
        escape(&view.display_filename)
    );
    let label = change_label(view);
    if !label.is_empty() {
        let _ = write!(html, " <span class=\"muted\">{label}</span>");
    }
    if let Some((tree, path)) = searchfox.zip(path_before(view)) {
        let _ = write!(
            html,
            " <a class=\"changeset-searchfox\" href=\"{}\">View in Searchfox</a>",
            escape(&tree.file_uri(path))
        );
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
        Body::Rows(rows) => rows_html(
            &mut html,
            rows,
            inlines,
            highlight::syntax_for(&view.display_filename),
        ),
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
        rows_html(&mut html, &rows, &[], None);
        assert!(
            html.contains("<td class=\"code\"><span class=\"marker\"> </span>same &lt;a&gt;</td>"),
            "{html}"
        );
        assert!(
            html.contains(
                "<tr class=\"diff-removed\"><td class=\"n\" data-side=\"old\" \
                 data-line=\"2\">2</td><td class=\"n\"></td>"
            ),
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

        // The same rows in a C file: the syntax inside the changed runs.
        let mut html = String::new();
        rows_html(&mut html, &rows, &[], highlight::syntax_for("a.c"));
        assert!(
            html.contains(
                "let x <span class=\"hl-operator\">=</span> \
                 <span class=\"bright\"><span class=\"hl-number\">2</span></span>;"
            ),
            "{html}"
        );
    }

    #[test]
    fn runs_never_cut_characters() {
        // The changed run ends in the middle of "é" (two bytes).
        let changed = cell(
            1,
            Some(LineType::Added),
            "x = \"é\";\n",
            vec![
                Segment::new(SegmentKind::Same, 4),
                Segment::new(SegmentKind::Changed, 2),
            ],
        );
        assert_eq!(
            cell_text(&changed, &[]),
            "x = <span class=\"bright\">&quot;é</span>&quot;;"
        );
        let tokens = [Token {
            range: 4..8,
            class: "hl-string",
        }];
        assert_eq!(
            cell_text(&changed, &tokens),
            "x = <span class=\"bright\"><span class=\"hl-string\">&quot;é</span></span>\
             <span class=\"hl-string\">&quot;</span>;"
        );
    }
}
