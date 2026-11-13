//! `PhutilRemarkupAnchorRule`: `{anchor #name}`.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static ANCHOR: Pattern = Pattern::new(r"(?s)\{anchor\s+#([^\s}]+)\}");
static NOT_ANCHOR: Pattern = Pattern::new(r"[\x00-\x2F\x3A-\x60\x7B-\x7F]+");

/// `PhutilRemarkupAnchorRule`.
pub struct Anchor;

impl InlineRule for Anchor {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&ANCHOR, text.as_bytes(), |groups| {
            if cx.is_text_mode() || cx.is_html_mail_mode() || cx.is_anchor_mode() {
                return Vec::new();
            }
            if !rule::is_flat_text(groups.get(0)) || !is_valid_anchor_name(groups.get(1)) {
                return groups.get(0).to_vec();
            }
            let tag = markup::tag(
                "a",
                &[("name", Some(groups.get(1)))],
                Content::One(&Markup::default()),
            );
            cx.store_text(tag)
        }))
    }
}

/// `isValidAnchorName()`.
pub fn is_valid_anchor_name(name: &[u8]) -> bool {
    normalize_anchor(name) == name
}

/// `normalizeAnchor()`: Latin characters other than `a-z` and `0-9`
/// become `-`; others (non-Latin letters, emoji) stay.
pub fn normalize_anchor(anchor: &[u8]) -> Vec<u8> {
    let anchor = preg::replace(&NOT_ANCHOR, anchor, b"-");
    php::trim_chars(&anchor, b"-").to_vec()
}
