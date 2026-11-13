//! `PhutilRemarkupTableBlockRule`: `<table>` blocks written in HTML.

use crate::blocks::table::{self, Cell, CellType};
use crate::engine::Context;
use crate::html::{Document, ROOT, Selected};
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static START: Pattern = Pattern::new(r"(?i)^\s*<table>");
static END: Pattern = Pattern::new(r"(?i)</table>\s*$");

/// `PhutilRemarkupTableBlockRule`.
pub struct HtmlTable;

impl BlockRule for HtmlTable {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupTableBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if !preg::is_match(&START, lines[cursor]) {
            return 0;
        }
        let mut num_lines = 1;
        for line in &lines[cursor + 1..] {
            num_lines += 1;
            if preg::is_match(&END, line) {
                break;
            }
        }
        num_lines
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let document = Document::parse(text);
        let mut out = Vec::new();
        let mut seen_table = false;
        for node in document.select_children_with_tags(ROOT, &[b"table"]) {
            match node {
                Selected::Content(content) => {
                    if php::is_blank(&content) {
                        continue;
                    }
                    // Text next to the table: render it all as text.
                    return Some(Markup::text(text));
                }
                Selected::Element(table) => {
                    if seen_table {
                        return Some(Markup::text(text));
                    }
                    seen_table = true;
                    out.push(Some(new_table(cx, &document, table)));
                }
            }
        }
        if cx.is_text_mode() {
            let bytes: Vec<u8> = out
                .iter()
                .flatten()
                .flat_map(|part| part.as_bytes().to_vec())
                .collect();
            return Some(Markup::Text(bytes));
        }
        Some(markup::implode_html(
            &Markup::default(),
            out.iter().map(Option::as_ref),
        ))
    }
}

/// `newTable()`: the rows of a `<table>`, or its raw text if it holds
/// anything else.
fn new_table(cx: &mut Context<'_>, document: &Document, table: usize) -> Markup {
    let raw = || Markup::Text(document.raw_string(table));
    let mut colgroup = false;
    let mut rows = Vec::new();
    for node in document.select_children_with_tags(table, &[b"colgroup", b"tr"]) {
        match node {
            Selected::Content(content) => {
                if php::is_blank(&content) {
                    continue;
                }
                return raw();
            }
            Selected::Element(element) if document.tag_name(element) == b"colgroup" => {
                // One colgroup, before the rows.
                if colgroup || !rows.is_empty() {
                    return raw();
                }
                colgroup = true;
            }
            Selected::Element(element) => rows.push(element),
        }
    }

    let mut row_specs = Vec::new();
    for row in rows {
        let mut cells = Vec::new();
        for cell in document.select_children_with_tags(row, &[b"td", b"th"]) {
            // Text between cells is skipped: PHP checks the wrong node
            // here, which is always empty by then.
            let Selected::Element(cell) = cell else {
                continue;
            };
            // Newlines in cells are line breaks.
            let content = document.raw_content_string(cell);
            let content = php::trim_chars(&content, b"\r\n");
            let lines: Vec<Option<Markup>> = split_lines_without_endings(content)
                .into_iter()
                .map(|line| Some(cx.apply_rules(Markup::text(line))))
                .collect();
            let br = markup::tag("br", &[], Content::None);
            let kind = if document.tag_name(cell) == b"th" {
                CellType::Header
            } else {
                CellType::Data
            };
            cells.push(Cell {
                kind,
                content: markup::implode_html(&br, lines.iter().map(Option::as_ref)),
            });
        }
        row_specs.push(cells);
    }
    table::render_table(cx, &row_specs)
}

/// `phutil_split_lines($text, false)`.
fn split_lines_without_endings(text: &[u8]) -> Vec<&[u8]> {
    if text.is_empty() {
        return vec![b""];
    }
    let mut lines: Vec<&[u8]> = text
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}
