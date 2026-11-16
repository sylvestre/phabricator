//! `PhabricatorKeyboardRemarkupRule`: `{key cmd shift P}` shows keys.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static KEYS: Pattern = Pattern::new(r"(?m)\{key\b((?:[^}\\]+|\\.)*)\}");
static ESCAPE: Pattern = Pattern::new(r"\\(.)");

/// The keys with a symbol: name, symbol and the names they are written
/// with.
static SPECIAL: &[(&str, &str, &[&str])] = &[
    ("Command", "\u{2318}", &["cmd", "command"]),
    ("Option", "\u{2325}", &["opt", "option"]),
    ("Shift", "\u{21e7}", &["shift"]),
    ("Escape", "\u{238b}", &["esc", "escape"]),
    ("Enter", "\u{23ce}", &["enter", "return"]),
    ("Control", "\u{2303}", &["ctrl", "control"]),
    ("Up", "\u{2191}", &["up", "arrow-up", "up-arrow", "north"]),
    ("Tab", "\u{21e5}", &["tab"]),
    (
        "Right",
        "\u{2192}",
        &["right", "right-arrow", "arrow-right", "east"],
    ),
    (
        "Left",
        "\u{2190}",
        &["left", "left-arrow", "arrow-left", "west"],
    ),
    (
        "Down",
        "\u{2193}",
        &["down", "down-arrow", "arrow-down", "south"],
    ),
    (
        "Up Right",
        "\u{2197}",
        &[
            "up-right",
            "upright",
            "up-right-arrow",
            "upright-arrow",
            "arrow-up-right",
            "arrow-upright",
            "northeast",
            "north-east",
        ],
    ),
    (
        "Down Right",
        "\u{2198}",
        &[
            "down-right",
            "downright",
            "down-right-arrow",
            "downright-arrow",
            "arrow-down-right",
            "arrow-downright",
            "southeast",
            "south-east",
        ],
    ),
    (
        "Down Left",
        "\u{2199}",
        &[
            "down-left",
            "downleft",
            "down-left-arrow",
            "downleft-arrow",
            "arrow-down-left",
            "arrow-downleft",
            "southwest",
            "south-west",
        ],
    ),
    (
        "Up Left",
        "\u{2196}",
        &[
            "up-left",
            "upleft",
            "up-left-arrow",
            "upleft-arrow",
            "arrow-up-left",
            "arrow-upleft",
            "northwest",
            "north-west",
        ],
    ),
];

const MAIL_KEY_STYLE: &str = "display: inline-block; min-width: 1em; padding: 4px 5px 5px; \
     font-weight: normal; font-size: 0.8rem; text-align: center; text-decoration: none; \
     line-height: 0.6rem; border-radius: 3px; box-shadow: inset 0 -1px 0 rgba(71, 87, 120, 0.08); \
     user-select: none; background: #f7f7f7; border: 1px solid #C7CCD9;";
const MAIL_JOIN_STYLE: &str = "padding: 0 4px; color: #92969D;";

/// `PhabricatorKeyboardRemarkupRule`.
pub struct Keyboard;

/// The name and symbol of a key as written.
fn key_spec(key: &[u8]) -> (Option<&'static str>, Vec<u8>) {
    let normal = php::utf8_strtolower(key);
    SPECIAL
        .iter()
        .find(|(_, _, aliases)| aliases.iter().any(|alias| alias.as_bytes() == normal))
        .map_or((None, key.to_vec()), |(name, symbol, _)| {
            (Some(*name), symbol.as_bytes().to_vec())
        })
}

impl InlineRule for Keyboard {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&KEYS, text.as_bytes(), |groups| {
            if !rule::is_flat_text(groups.get(0)) {
                return groups.get(0).to_vec();
            }
            let keys: Vec<Vec<u8>> = groups
                .get(1)
                .split(|byte| *byte == b' ')
                .map(|key| preg::replace(&ESCAPE, php::trim_chars(key, b" \n"), b"\\1"))
                .filter(|key| !key.is_empty())
                .collect();
            let specs: Vec<(Option<&str>, Vec<u8>)> =
                keys.iter().map(|key| key_spec(key)).collect();
            if cx.is_text_mode() {
                let parts: Vec<Vec<u8>> = specs
                    .iter()
                    .map(|(_, symbol)| [b"[", &symbol[..], b"]"].concat())
                    .collect();
                return cx.store_text(Markup::Text(php::implode(b" + ", &parts)));
            }
            let mail = cx.is_html_mail_mode();
            let key_style = mail.then_some(MAIL_KEY_STYLE.as_bytes());
            let join_style = mail.then_some(MAIL_JOIN_STYLE.as_bytes());
            let parts: Vec<Option<Markup>> = specs
                .iter()
                .map(|(name, symbol)| {
                    Some(markup::tag(
                        "kbd",
                        &[("title", name.map(str::as_bytes)), ("style", key_style)],
                        Content::One(&Markup::text(symbol.clone())),
                    ))
                })
                .collect();
            let glue = markup::tag(
                "span",
                &[("class", Some(b"kbd-join")), ("style", join_style)],
                Content::One(&Markup::text("+")),
            );
            let html = markup::implode_html(&glue, parts.iter().map(Option::as_ref));
            cx.store_text(html)
        }))
    }
}
