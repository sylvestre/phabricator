//! `PhabricatorIconRemarkupRule`: `{icon camera color=red}` shows a Font
//! Awesome icon.

use crate::engine::Context;
use crate::generated::icons::{COLORS, ICONS};
use crate::markup::{self, Content, Markup};
use crate::options::{self, OptionValue};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static ICON: Pattern = Pattern::new(r"(?m)\{icon\b((?:[^}\\]+|\\.)*)\}");
static SEPARATOR: Pattern = Pattern::new(r"[\s,]+");

/// `PhabricatorIconRemarkupRule`.
pub struct Icon;

/// Whether `fa-<name>` is an icon of `PHUIIconView::getIcons()`.
pub fn is_icon(name: &[u8]) -> bool {
    let name = [b"fa-", name].concat();
    std::str::from_utf8(&name).is_ok_and(|name| ICONS.binary_search(&name).is_ok())
}

/// The icon `fa-<name>`, which must be valid ([`is_icon()`]), with a
/// valid colour.
pub fn render(name: &[u8], color: Option<&[u8]>, spin: bool) -> Markup {
    let mut class = [b"fa fa-", name, b" remarkup-icon"].concat();
    if let Some(color) = color {
        class.extend_from_slice(b" remarkup-icon-");
        class.extend_from_slice(color);
    }
    if spin {
        class.extend_from_slice(b" fa-spin");
    }
    markup::tag(
        "span",
        &[("class", Some(&class)), ("aria-hidden", Some(b"true"))],
        Content::None,
    )
}

fn is_color(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(|name| COLORS.binary_search(&name).is_ok())
}

impl InlineRule for Icon {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&ICON, text.as_bytes(), |groups| {
            if !rule::is_flat_text(groups.get(0)) || cx.is_text_mode() || cx.is_html_mail_mode() {
                return groups.get(0).to_vec();
            }
            let extra = php::ltrim(groups.get(1), b", \n");
            // preg_split() with a limit of 2: the name, then the options.
            let (icon, rest) = match preg::captures(&SEPARATOR, extra) {
                Some(separator) => (&extra[..separator.start()], &extra[separator.end()..]),
                None => (extra, &b""[..]),
            };
            // Previews show some icon while the name is typed.
            let icon = if is_icon(icon) { icon } else { b"paw" };
            let parsed = options::parse(rest);
            let color = match parsed.get(&b"color"[..]) {
                Some(OptionValue::Value(color)) if is_color(color) => Some(color.as_slice()),
                _ => None,
            };
            let spin = parsed.get(&b"spin"[..]).is_some_and(OptionValue::is_truthy);
            cx.store_text(render(icon, color, spin))
        }))
    }
}
