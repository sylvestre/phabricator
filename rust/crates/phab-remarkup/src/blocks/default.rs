//! `PhutilRemarkupDefaultBlockRule`: paragraphs.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static WRAP: Pattern = Pattern::new(r" *\n *");

/// `PhutilRemarkupDefaultBlockRule`: every line is a paragraph line;
/// consecutive ones merge.
pub struct Paragraph;

impl BlockRule for Paragraph {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupDefaultBlockRule"
    }

    fn priority(&self) -> i32 {
        750
    }

    fn is_default(&self) -> bool {
        true
    }

    fn matching_line_count(&self, _lines: &[&[u8]], _cursor: usize) -> usize {
        1
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let preserve_linebreaks = cx.config().preserve_linebreaks;
        let text = cx.apply_rules(Markup::text(php::trim(text)));
        if cx.is_text_mode() {
            if preserve_linebreaks {
                return Some(text);
            }
            return Some(Markup::Text(preg::replace(&WRAP, text.as_bytes(), b" ")));
        }
        let text = if preserve_linebreaks {
            markup::escape_newlines(&text)
        } else {
            text
        };
        if text.is_empty() {
            return None;
        }
        Some(markup::tag("p", &[], Content::One(&text)))
    }
}
