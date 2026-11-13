//! `PhutilRemarkupLinebreaksRule`.

use crate::engine::Context;
use crate::markup::{self, Markup};
use crate::rule::InlineRule;

/// `PhutilRemarkupLinebreaksRule`: newlines become `<br />`.
pub struct Linebreaks;

impl InlineRule for Linebreaks {
    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if cx.is_text_mode() {
            return text;
        }
        markup::escape_newlines(&text)
    }
}
