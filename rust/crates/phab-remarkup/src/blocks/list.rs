//! `PhutilRemarkupListBlockRule`: `-`, `*`, `#`, `1.` and `[ ]` lists,
//! nested by indentation or by repeating the marker.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::{self, BlockRule};

static START_BLOCK: Pattern = Pattern::new(r"^\s*(?:[-*#]+|([1-9][0-9]*)[.)]|\[\D?\])\s+");
static CONT_BLOCK: Pattern = Pattern::new(r"^\s*(?:[-*#]+|[0-9]+[.)]|\[\D?\])\s+");
static STRIP_BLOCK: Pattern = Pattern::new(r"^\s*(?:[-*#]+|[0-9]+[.)])\s*");
static LEADING_SPACE: Pattern = Pattern::new(r"^(\s+)");
static LINE_INDENT: Pattern = Pattern::new(r"^ ");
static WRAP_SPACES: Pattern = Pattern::new(r" *(\n+) *");
static SINGLE_NEWLINE: Pattern = Pattern::new(r"(?<!\n)\n(?!\n)");
static ALTERNATE_DEPTH: Pattern = Pattern::new(r"^\s*([-*#]{2,})");
static ORDERED: Pattern = Pattern::new(r"^\s*(?:#|[0-9])");
static CHECKBOX: Pattern = Pattern::new(r"^\s*\[(\D?)\]\s*");
static PARAGRAPHS: Pattern = Pattern::new(r"\n{2,}");
static TRAILING_SPACES: Pattern = Pattern::new(r"(?m) +$");

/// `MAXIMUM_LIST_NESTING_DEPTH`.
const MAXIMUM_NESTING_DEPTH: usize = 12;

/// `-` lists or `#` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Unordered,
    Ordered,
}

/// A list item, or a placeholder (`text: None`) where the tree needs a
/// level the input skipped.
#[derive(Debug, Clone)]
struct Node {
    text: Option<Vec<u8>>,
    depth: usize,
    style: Option<Style>,
    mark: Option<bool>,
    items: Vec<Self>,
}

/// `PhutilRemarkupListBlockRule`.
pub struct List;

impl BlockRule for List {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupListBlockRule"
    }

    /// Before code blocks, so `  - item` is a list.
    fn priority(&self) -> i32 {
        400
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        let first_line = cursor;
        let mut cursor = cursor;
        let mut num_lines = 0;
        let mut is_one_line = false;
        while cursor < lines.len() {
            if num_lines == 0 {
                if preg::is_match(&START_BLOCK, lines[cursor]) {
                    num_lines += 1;
                    cursor += 1;
                    is_one_line = true;
                    continue;
                }
            } else {
                if preg::is_match(&CONT_BLOCK, lines[cursor]) {
                    num_lines += 1;
                    cursor += 1;
                    is_one_line = false;
                    continue;
                }
                // Lists continue across paragraphs while lines are indented
                // or one empty line separates indented lines.
                let this_empty = php::is_blank(lines[cursor]);
                let this_indented = preg::is_match(&LINE_INDENT, lines[cursor]);
                let (next_empty, next_indented) =
                    lines.get(cursor + 1).map_or((true, false), |next| {
                        (php::is_blank(next), preg::is_match(&LINE_INDENT, next))
                    });
                let continues =
                    (this_indented && !this_empty) || (this_empty && next_indented && !next_empty);
                if continues {
                    num_lines += 1;
                    cursor += 1;
                    continue;
                }
                if this_empty {
                    num_lines += 1;
                }
            }
            break;
        }

        // A single "#" item which is not the last line is a header.
        if is_one_line
            && first_line + num_lines < lines.len()
            && lines[first_line].starts_with(b"#")
        {
            return 0;
        }
        num_lines
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let mut lines: Vec<Vec<u8>> = php::explode_lines(text)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();

        // Un-indent every item by the smallest indentation, so marker-count
        // nesting ("- a", "-- b") works in an indented list.
        let mut pattern = &START_BLOCK;
        let mut min_space = usize::MAX;
        for line in &lines {
            if preg::is_match(pattern, line) {
                pattern = &CONT_BLOCK;
                let space =
                    preg::captures(&LEADING_SPACE, line).map_or(0, |groups| groups.get(1).len());
                min_space = min_space.min(space);
            }
        }
        let mut pattern = &START_BLOCK;
        if min_space != usize::MAX && min_space > 0 {
            for line in &mut lines {
                if preg::is_match(pattern, line) {
                    pattern = &CONT_BLOCK;
                    *line = line.get(min_space..).unwrap_or_default().to_vec();
                }
            }
        }

        // Group wrapped lines into items.
        let mut groups: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut item: Vec<Vec<u8>> = Vec::new();
        let mut starts_at: Option<Vec<u8>> = None;
        let mut pattern = &START_BLOCK;
        for line in lines {
            if let Some(found) = preg::captures(pattern, &line) {
                if starts_at.is_none() && !found.get(1).is_empty() && found.get(1) != b"0" {
                    starts_at = Some(found.get(1).to_vec());
                }
                pattern = &CONT_BLOCK;
                if !item.is_empty() {
                    groups.push(std::mem::take(&mut item));
                }
            }
            item.push(line);
        }
        if !item.is_empty() {
            groups.push(item);
        }
        let starts_at = starts_at.unwrap_or_else(|| b"1".to_vec());

        // Normalize each item: unwrap lines, find its depth, style and mark.
        let mut has_marks = false;
        let mut items = Vec::new();
        for group in groups {
            let item = preg::replace(&WRAP_SPACES, &php::implode(b"\n", &group), b"\\1");
            let item = preg::replace(&SINGLE_NEWLINE, &item, b" ");
            let item = php::rtrim(&item, php::WHITESPACE);
            if item.is_empty() {
                continue;
            }
            let depth = if let Some(found) = preg::captures(&ALTERNATE_DEPTH, item) {
                found.get(1).len() - 1
            } else if let Some(found) = preg::captures(&LEADING_SPACE, item) {
                found.get(1).len()
            } else {
                0
            };
            let style = if preg::is_match(&ORDERED, item) {
                Style::Ordered
            } else {
                Style::Unordered
            };
            let mut text = preg::replace(&STRIP_BLOCK, item, b"");
            let mut mark = None;
            if let Some(found) = preg::captures(&CHECKBOX, &text) {
                mark = Some(!php::trim(found.get(1)).is_empty());
                has_marks = true;
                text = text[found.get(0).len()..].to_vec();
            }
            items.push(Node {
                text: Some(text),
                depth,
                style: Some(style),
                mark,
                items: Vec::new(),
            });
        }

        let mut tree = build_tree(&items, 0, items.len(), 0);
        adjust_tree_style(&mut tree);
        let out = render_tree(cx, &tree, 0, has_marks, &starts_at);

        if cx.is_text_mode() {
            let out: Vec<u8> = out
                .iter()
                .flat_map(|part| part.as_bytes().to_vec())
                .collect();
            let out = php::rtrim(&out, b"\n");
            return Some(Markup::Text(preg::replace(&TRAILING_SPACES, out, b"")));
        }
        let parts: Vec<Option<Markup>> = out.into_iter().map(Some).collect();
        Some(markup::implode_html(
            &Markup::default(),
            parts.iter().map(Option::as_ref),
        ))
    }
}

/// `buildTree()`: nests items by depth, with placeholders where the input
/// skips a level.
fn build_tree(items: &[Node], left: usize, right: usize, level: usize) -> Vec<Node> {
    if left == right {
        return Vec::new();
    }
    if level > MAXIMUM_NESTING_DEPTH {
        return items[left..right].to_vec();
    }

    // The last item with the smallest depth (ties go to the earliest).
    let mut min = left;
    for index in (left..right).rev() {
        if items[index].depth <= items[min].depth {
            min = index;
        }
    }
    let min_depth = items[min].depth;

    let mut nodes = Vec::new();
    if min != left {
        nodes.push(Node {
            text: None,
            depth: 0,
            style: None,
            mark: None,
            items: build_tree(items, left, min, level + 1),
        });
    }
    let mut last = min;
    for index in last + 1..right {
        if items[index].depth == min_depth {
            let mut node = items[last].clone();
            node.items = build_tree(items, last + 1, index, level + 1);
            nodes.push(node);
            last = index;
        }
    }
    let mut node = items[last].clone();
    node.items = build_tree(items, last + 1, right, level + 1);
    nodes.push(node);
    nodes
}

/// `adjustTreeStyleInformation()`: placeholders take the style of the next
/// item at their level.
fn adjust_tree_style(tree: &mut [Node]) {
    let mut style = Style::Unordered;
    for node in tree.iter_mut().rev() {
        match node.style {
            Some(own) => style = own,
            None => node.style = Some(style),
        }
        if !node.items.is_empty() {
            adjust_tree_style(&mut node.items);
        }
    }
}

/// `renderTree()`.
fn render_tree(
    cx: &mut Context<'_>,
    tree: &[Node],
    level: usize,
    has_marks: bool,
    starts_at: &[u8],
) -> Vec<Markup> {
    let style = tree.first().and_then(|node| node.style);
    let text_mode = cx.is_text_mode();
    let mut out = Vec::new();

    let tag = match style {
        Some(Style::Ordered) => "ol",
        _ => "ul",
    };
    if !text_mode {
        // `ctype_digit($starts_at) && $starts_at > 1`.
        let start = !starts_at.is_empty()
            && starts_at.iter().all(u8::is_ascii_digit)
            && parse_start(starts_at) > 1;
        let start_attribute = if start {
            format!(" start=\"{}\"", parse_start(starts_at))
        } else {
            String::new()
        };
        let class = if has_marks {
            "remarkup-list remarkup-list-with-checkmarks"
        } else {
            "remarkup-list"
        };
        out.push(Markup::html(format!(
            "<{tag} class=\"{class}\"{start_attribute}>"
        )));
        out.push(Markup::text("\n"));
    }

    let mut number = parse_start(starts_at);
    for node in tree {
        if text_mode {
            if let Some(text) = &node.text {
                render_text_item(cx, &mut out, node, text, level, style, &mut number);
            }
        } else {
            render_html_item(cx, &mut out, node);
        }

        if !node.items.is_empty() {
            out.extend(render_tree(cx, &node.items, level + 1, has_marks, b"1"));
        }
        if !text_mode {
            out.push(rule::hsprintf("</li>\n", &[]));
        }
    }

    if !text_mode {
        out.push(Markup::html(format!("</{tag}>")));
    }
    out
}

/// One item in text mode: indent, marker, then paragraphs.
fn render_text_item(
    cx: &mut Context<'_>,
    out: &mut Vec<Markup>,
    node: &Node,
    text: &[u8],
    level: usize,
    style: Option<Style>,
    number: &mut i64,
) {
    let indent = " ".repeat(2 * level);
    out.push(Markup::text(indent.clone()));
    match node.mark {
        Some(true) => out.push(Markup::text("[X] ")),
        Some(false) => out.push(Markup::text("[ ] ")),
        None if style == Some(Style::Ordered) => {
            out.push(Markup::text(format!("{number}. ")));
            *number += 1;
        }
        None => out.push(Markup::text("- ")),
    }
    for (index, part) in preg::split(&PARAGRAPHS, text).into_iter().enumerate() {
        if index != 0 {
            out.push(Markup::text(format!("\n\n  {indent}")));
        }
        out.push(cx.apply_rules(Markup::text(part)));
    }
    out.push(Markup::text("\n"));
}

/// The opening `<li>` and content of one item in HTML.
fn render_html_item(cx: &mut Context<'_>, out: &mut Vec<Markup>, node: &Node) {
    let Some(text) = &node.text else {
        out.push(Markup::html(
            "<li class=\"remarkup-list-item phantom-item\">",
        ));
        return;
    };
    match node.mark {
        Some(checked) => {
            out.push(Markup::html(if checked {
                "<li class=\"remarkup-list-item remarkup-checked-item\">"
            } else {
                "<li class=\"remarkup-list-item remarkup-unchecked-item\">"
            }));
            out.push(markup::tag(
                "input",
                &[
                    ("type", Some(b"checkbox")),
                    ("checked", checked.then_some(b"checked")),
                    ("disabled", Some(b"disabled")),
                ],
                Content::None,
            ));
            out.push(Markup::text(" "));
        }
        None => out.push(Markup::html("<li class=\"remarkup-list-item\">")),
    }
    for (index, part) in preg::split(&PARAGRAPHS, text).into_iter().enumerate() {
        if index != 0 {
            let br = markup::tag("br", &[], Content::None);
            let newline = Markup::text("\n");
            out.push(markup::concat([
                Some(&newline),
                Some(&br),
                Some(&br),
                Some(&newline),
            ]));
        }
        out.push(cx.apply_rules(Markup::text(part)));
    }
}

/// The number a list starts at, from `[1-9][0-9]*`; like PHP's `(int)`,
/// it stops at `i64::MAX`.
fn parse_start(starts_at: &[u8]) -> i64 {
    starts_at
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .fold(0i64, |value, digit| {
            value
                .saturating_mul(10)
                .saturating_add(i64::from(digit - b'0'))
        })
}
