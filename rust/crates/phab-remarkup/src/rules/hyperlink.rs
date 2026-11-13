//! `PhutilRemarkupHyperlinkRule`: `<uri>`, `{uri}` and bare URIs. Links are
//! stored as tokens while marking up and rendered at the end, once every
//! link of the text is known.

use crate::engine::{Context, Hyperlink as Link, LinkMode};
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};
use crate::uri;

// See T13608: the protocol length is bounded, and the bare pattern has a
// lookbehind, to keep matching fast on long inputs.
static ANGLE: Pattern = Pattern::new(r"<(\w{3,32}://[^\s\x01]+?)>");
static CURLY: Pattern = Pattern::new(r"\{(\w{3,32}://[^\s\x01]+?)\}");
static BARE: Pattern = Pattern::new(r"(?<!\w)\w{3,32}://[^\s\x01]+");
static TRAILING_PUNCTUATION: Pattern = Pattern::new(r"[;,.:!?]+$");

/// Decides what a link which no special handler took renders as. The
/// Phabricator engine adds handlers for links to itself; the generic
/// engine has none.
pub trait LinkHandler: Send + Sync {
    /// The rendering of `uri`, or `None` to leave it to the next handler.
    fn render(&self, cx: &Context<'_>, uri: &[u8], embed: bool) -> Option<Markup>;
}

/// `PhutilRemarkupHyperlinkRule`.
#[derive(Default)]
pub struct Hyperlink {
    handlers: Vec<Box<dyn LinkHandler>>,
}

impl Hyperlink {
    /// The rule with these handlers (`PhutilRemarkupHyperlinkEngineExtension`).
    pub fn with_handlers(handlers: Vec<Box<dyn LinkHandler>>) -> Self {
        Self { handlers }
    }
}

fn markup_hyperlink(cx: &mut Context<'_>, mode: LinkMode, uri: &[u8], whole: &[u8]) -> Vec<u8> {
    if uri::protocol(uri).is_err() {
        return whole.to_vec();
    }
    let token = cx.store_text(Markup::text(uri));
    cx.metadata.hyperlinks.push(Link {
        token: token.clone(),
        uri: uri.to_vec(),
        mode,
    });
    token
}

impl InlineRule for Hyperlink {
    fn priority(&self) -> f64 {
        400.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        // "<uri>" links exactly what is between the brackets.
        let text = preg::replace_callback(&ANGLE, text.as_bytes(), |groups| {
            markup_hyperlink(cx, LinkMode::Literal, groups.get(1), groups.get(0))
        });
        // "{uri}" is matched, but only linked by a handler.
        let text = preg::replace_callback(&CURLY, &text, |groups| {
            markup_hyperlink(cx, LinkMode::Embed, groups.get(1), groups.get(0))
        });
        // Bare URIs leave out trailing punctuation, and a closing
        // parenthesis without an opening one: "(see http://x.com)".
        let text = preg::replace_callback(&BARE, &text, |groups| {
            let whole = groups.get(0);
            let mut link = whole;
            let mut tail = Vec::new();
            if let Some(trailing) = preg::captures(&TRAILING_PUNCTUATION, link) {
                tail = trailing.get(0).to_vec();
                link = &link[..link.len() - tail.len()];
            }
            if link.ends_with(b")") && !link.contains(&b'(') {
                tail.insert(0, b')');
                link = &link[..link.len() - 1];
            }
            if uri::protocol(link).is_err() {
                return whole.to_vec();
            }
            let token = markup_hyperlink(cx, LinkMode::Bare, link, whole);
            rule::hsprintf("%s%s", &[&Markup::Text(token), &Markup::Text(tail)]).into_bytes()
        });
        Markup::Text(text)
    }

    fn did_markup_text(&self, cx: &mut Context<'_>) {
        let links = std::mem::take(&mut cx.metadata.hyperlinks);
        let toc_or_text = cx.state("toc") || cx.is_text_mode();
        let mut remaining = Vec::new();
        for link in &links {
            let embed = link.mode == LinkMode::Embed;
            // Tables of contents and text get the URI as written.
            if toc_or_text {
                cx.overwrite_stored_text(&link.token, raw_link(&link.uri, embed));
                continue;
            }
            // Only allowed protocols are linked ("javascript://" is not).
            let protocol = uri::protocol(&link.uri).unwrap_or_default();
            if !cx
                .config()
                .allowed_protocols
                .contains(String::from_utf8_lossy(&protocol).as_ref())
            {
                cx.overwrite_stored_text(&link.token, raw_link(&link.uri, embed));
                continue;
            }
            // "<uri>", and every link in mail, is rendered literally.
            if link.mode == LinkMode::Literal || cx.is_html_mail_mode() {
                let rendered = render_hyperlink(cx, &link.uri, embed);
                cx.overwrite_stored_text(&link.token, rendered);
                continue;
            }
            remaining.push(link);
        }
        for link in remaining {
            let embed = link.mode == LinkMode::Embed;
            let rendered = self
                .handlers
                .iter()
                .find_map(|handler| handler.render(cx, &link.uri, embed))
                .unwrap_or_else(|| render_hyperlink(cx, &link.uri, embed));
            cx.overwrite_stored_text(&link.token, rendered);
        }
        cx.metadata.hyperlinks = links;
    }
}

/// `renderRawLink()`.
fn raw_link(uri: &[u8], embed: bool) -> Markup {
    if embed {
        Markup::Text([b"{", uri, b"}"].concat())
    } else {
        Markup::text(uri)
    }
}

/// `renderHyperlink()`: an embed nobody handled is shown as written.
pub fn render_hyperlink(cx: &Context<'_>, uri: &[u8], embed: bool) -> Markup {
    if embed {
        return raw_link(uri, embed);
    }
    let target: Option<&[u8]> = (!cx.config().same_window).then_some(b"_blank");
    markup::tag(
        "a",
        &[
            ("href", Some(uri)),
            ("class", Some(b"remarkup-link")),
            ("target", target),
            ("rel", Some(b"noreferrer")),
        ],
        Content::One(&Markup::text(uri)),
    )
}
