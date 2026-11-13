//! `PhutilRemarkupLiteralBlockRule`: `%%%literal text%%%`.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static START: Pattern = Pattern::new(r"^\s*%%%");
static END: Pattern = Pattern::new(r"%%%\s*$");
static TRIVIAL: Pattern = Pattern::new(r"^\s*%%%\s*$");
static STRIP_START: Pattern = Pattern::new(r"^\s*%%%");
static STRIP_END: Pattern = Pattern::new(r"%%%(\s*)\z");

/// `PhutilRemarkupLiteralBlockRule`.
pub struct Literal;

impl BlockRule for Literal {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupLiteralBlockRule"
    }

    fn priority(&self) -> i32 {
        450
    }

    /// Consecutive `%%%` blocks merge, unless an empty line separates them.
    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if !preg::is_match(&START, lines[cursor]) {
            return 0;
        }
        let start_cursor = cursor;
        let mut cursor = cursor;
        let mut found_empty = false;
        let mut block_start: Option<usize> = None;
        while cursor < lines.len() {
            let line = lines[cursor];
            if block_start.is_none() {
                if found_empty {
                    break;
                }
                if preg::is_match(&START, line) {
                    block_start = Some(cursor);
                }
            }
            if let Some(start) = block_start {
                // A line with only "%%%" starts a block; it doesn't end it.
                let is_end = preg::is_match(&END, line)
                    && !(cursor == start && preg::is_match(&TRIVIAL, line));
                if is_end {
                    block_start = None;
                    cursor += 1;
                    continue;
                }
            }
            if block_start.is_none() {
                if !php::is_blank(line) {
                    break;
                }
                found_empty = true;
            }
            cursor += 1;
        }
        cursor - start_cursor
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let text = php::rtrim(text, php::WHITESPACE);
        let lines: Vec<Vec<u8>> = php::split_lines(text)
            .into_iter()
            .map(|line| {
                let line = preg::replace(&STRIP_START, line, b"");
                preg::replace(&STRIP_END, &line, b"\\1")
            })
            .collect();
        if cx.is_text_mode() {
            return Some(Markup::Text(lines.concat()));
        }
        let lines: Vec<Option<Markup>> = lines
            .into_iter()
            .map(|line| Some(Markup::Text(line)))
            .collect();
        let br = markup::tag("br", &[], Content::None);
        let content = markup::implode_html(&br, lines.iter().map(Option::as_ref));
        Some(markup::tag(
            "p",
            &[("class", Some(b"remarkup-literal"))],
            Content::One(&content),
        ))
    }
}
