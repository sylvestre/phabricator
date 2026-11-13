//! `PhutilRemarkupEscapeRemarkupRule`: a literal `\x01` in the input would
//! look like a token, so it is stored as a token itself.

use crate::engine::Context;
use crate::markup::Markup;
use crate::rule::InlineRule;
use crate::storage::MAGIC_BYTE;

/// `PhutilRemarkupEscapeRemarkupRule`.
pub struct EscapeRemarkup;

impl InlineRule for EscapeRemarkup {
    fn priority(&self) -> f64 {
        0.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if !text.as_bytes().contains(&MAGIC_BYTE) {
            return text;
        }
        let token = cx.store_text(Markup::text([MAGIC_BYTE]));
        let mut output = Vec::new();
        for &byte in text.as_bytes() {
            if byte == MAGIC_BYTE {
                output.extend_from_slice(&token);
            } else {
                output.push(byte);
            }
        }
        Markup::Text(output)
    }
}
