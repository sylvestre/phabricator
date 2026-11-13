//! `PhutilRemarkupMonospaceRule`: `##text##` and `` `text` ``.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule::InlineRule;

static MONOSPACE: Pattern = Pattern::new(r"##([\s\S]+?)##|\B`(.+?)`");

/// `PhutilRemarkupMonospaceRule`.
pub struct Monospace;

impl InlineRule for Monospace {
    fn priority(&self) -> f64 {
        100.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(
            &MONOSPACE,
            text.as_bytes(),
            |groups| {
                let result = if cx.is_text_mode() {
                    Markup::text(groups.get(0))
                } else {
                    let matched = Markup::text(if groups.is_set(2) {
                        groups.get(2)
                    } else {
                        groups.get(1)
                    });
                    let attributes: &[(&str, Option<&[u8]>)] = if cx.is_html_mail_mode() {
                        &[("style", Some(b"background: #ebebeb; font-size: 13px;"))]
                    } else {
                        &[("class", Some(b"remarkup-monospaced"))]
                    };
                    markup::tag("tt", attributes, Content::One(&matched))
                };
                cx.store_text(result)
            },
        ))
    }
}
