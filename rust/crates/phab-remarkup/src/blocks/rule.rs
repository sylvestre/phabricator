//! `PhutilRemarkupHorizontalRuleBlockRule`: `---`, `***` and `___`.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static RULE: Pattern = Pattern::new(r"^\s*(?:_{3,}|\*\s?\*\s?\*(\s|\*)*|\-\s?\-\s?\-(\s|\-)*)$");

/// `PhutilRemarkupHorizontalRuleBlockRule`.
pub struct HorizontalRule;

impl BlockRule for HorizontalRule {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupHorizontalRuleBlockRule"
    }

    /// Before lists, so `---` is a rule and not an item.
    fn priority(&self) -> i32 {
        300
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if !preg::is_match(&RULE, php::rtrim(lines[cursor], b"\n\r")) {
            return 0;
        }
        1 + blank_lines_after(lines, cursor + 1)
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        if cx.is_text_mode() {
            return Some(Markup::text(php::rtrim(text, php::WHITESPACE)));
        }
        Some(markup::tag(
            "hr",
            &[("class", Some(b"remarkup-hr"))],
            Content::None,
        ))
    }
}

/// How many blank lines follow from `cursor`.
pub(crate) fn blank_lines_after(lines: &[&[u8]], cursor: usize) -> usize {
    lines[cursor.min(lines.len())..]
        .iter()
        .take_while(|line| php::is_blank(line))
        .count()
}
