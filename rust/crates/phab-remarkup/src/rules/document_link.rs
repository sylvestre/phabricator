//! `PhutilRemarkupDocumentLinkRule`: `[[ href | name ]]` and
//! `[name](href)` links.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};
use crate::{php, uri};

static WIKI_LINK: Pattern = Pattern::new(r"(?U)\B\[\[([^|\]]+)(?:\|([^\]]+))?\]\]\B");
static MARKDOWN_LINK: Pattern =
    Pattern::new(r"(?U)\B\[([^\]]+)\]\((\s*[^\s()]*(?:(?:\([^ ()]*\))+[^\s()]*)?\s*)\)\B");
static IS_URI: Pattern = Pattern::new(r"(^/)|(://)|(^#)|(^(?:mailto|tel):)");
static MAIL_OR_TEL: Pattern = Pattern::new(r"^(?:mailto|tel):");

/// `PhutilRemarkupDocumentLinkRule`.
pub struct DocumentLink;

impl InlineRule for DocumentLink {
    fn priority(&self) -> f64 {
        150.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        let text = preg::replace_callback(&WIKI_LINK, text.as_bytes(), |groups| {
            markup_document_link(cx, groups.get(0), groups.get(1), groups.get(2))
        });
        let text = preg::replace_callback(&MARKDOWN_LINK, &text, |groups| {
            markup_alternate_link(cx, groups.get(0), groups.get(1), groups.get(2))
        });
        Markup::Text(text)
    }
}

/// `markupAlternateLink()`: `[name](uri)` only links URIs which look like
/// URIs, so `x[0][1](y)` in C code stays text.
fn markup_alternate_link(cx: &mut Context<'_>, whole: &[u8], name: &[u8], uri: &[u8]) -> Vec<u8> {
    let trimmed = php::trim(uri);
    if trimmed.is_empty() || matches!(trimmed[0], b'"' | b'\'' | b'`') {
        return whole.to_vec();
    }
    if !trimmed.contains(&b'/') && !trimmed.contains(&b'@') && !trimmed.starts_with(b"tel:") {
        return whole.to_vec();
    }
    markup_document_link(cx, whole, uri, name)
}

/// `markupDocumentLink()`.
fn markup_document_link(cx: &mut Context<'_>, whole: &[u8], uri: &[u8], name: &[u8]) -> Vec<u8> {
    let uri = php::trim(uri);
    let name = php::trim(name);
    if !rule::is_flat_text(uri) || !rule::is_flat_text(name) {
        return whole.to_vec();
    }

    // Paths, anchors, "://" URIs and mail or phone links are URIs; anything
    // else is a wiki page, which Phriction would handle.
    let mut is_uri = preg::is_match(&IS_URI, uri);
    if is_uri && !uri.starts_with(b"/") && !uri.starts_with(b"#") {
        is_uri = match uri::protocol(uri) {
            Ok(protocol) => cx
                .config()
                .allowed_protocols
                .contains(String::from_utf8_lossy(&protocol).as_ref()),
            Err(uri::InvalidUri) => false,
        };
    }
    // "[[ / ]]" is the Phriction root, not the home page.
    if uri == b"/" {
        is_uri = false;
    }
    if !is_uri {
        return whole.to_vec();
    }
    let link = render_hyperlink(cx, uri, name);
    cx.store_text(link)
}

/// `renderHyperlink()`.
fn render_hyperlink(cx: &Context<'_>, link: &[u8], name: &[u8]) -> Markup {
    let config = cx.config();
    let mut is_anchor = false;
    let link = if link.starts_with(b"/") {
        let base = config.uri_base.as_deref().unwrap_or("");
        [php::rtrim(base.as_bytes(), b"/"), link].concat()
    } else if link.starts_with(b"#") {
        is_anchor = true;
        [config.uri_here.as_deref().unwrap_or("").as_bytes(), link].concat()
    } else {
        link.to_vec()
    };

    if cx.is_text_mode() {
        let link = preg::replace(&MAIL_OR_TEL, &link, b"");
        if name.is_empty() {
            return Markup::Text(link);
        }
        return Markup::Text([name, b" <", &link, b">"].concat());
    }

    let name = if name.is_empty() {
        preg::replace(&MAIL_OR_TEL, &link, b"")
    } else {
        name.to_vec()
    };
    if cx.state("toc") {
        return Markup::Text(name);
    }
    // Anchors on the same page always stay in the window.
    let target: Option<&[u8]> = (!config.same_window && !is_anchor).then_some(b"_blank");
    markup::tag(
        "a",
        &[
            ("href", Some(&link)),
            ("class", Some(b"remarkup-link")),
            ("target", target),
            ("rel", Some(b"noreferrer")),
        ],
        Content::One(&Markup::Text(name)),
    )
}
