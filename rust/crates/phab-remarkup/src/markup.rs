//! Values of the Remarkup engine: plain text, which is escaped when it
//! reaches HTML, or safe HTML (`PhutilSafeHTML`), which is not. PHP tells
//! them apart by type; the engine depends on which one each rule returns.

use crate::php;

/// A string or a `PhutilSafeHTML`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Markup {
    /// A plain PHP string.
    Text(Vec<u8>),
    /// A `PhutilSafeHTML`.
    Html(Vec<u8>),
}

impl Markup {
    /// A plain string.
    pub fn text(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Text(bytes.into())
    }

    /// Safe HTML.
    pub fn html(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Html(bytes.into())
    }

    /// `(string)$value`: the bytes, whether escaped or not.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(bytes) | Self::Html(bytes) => bytes,
        }
    }

    /// The bytes, whether escaped or not.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Text(bytes) | Self::Html(bytes) => bytes,
        }
    }

    /// `phutil_escape_html()`: safe HTML as is, text escaped.
    pub fn escape(&self) -> Vec<u8> {
        match self {
            Self::Text(bytes) => php::htmlspecialchars(bytes),
            Self::Html(bytes) => bytes.clone(),
        }
    }

    /// `phutil_safe_html()`: marks text as safe, except an empty string,
    /// which PHP leaves a (falsy) string.
    pub fn into_safe(self) -> Self {
        match self {
            Self::Text(bytes) | Self::Html(bytes) if bytes.is_empty() => Self::Text(bytes),
            Self::Text(bytes) => Self::Html(bytes),
            html @ Self::Html(_) => html,
        }
    }

    /// `phutil_escape_html()` as a safe value.
    pub fn into_escaped(self) -> Self {
        match self {
            Self::Text(bytes) => Self::Html(php::htmlspecialchars(&bytes)),
            html @ Self::Html(_) => html,
        }
    }

    /// Whether the value is an empty string.
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }
}

impl Default for Markup {
    fn default() -> Self {
        Self::Text(Vec::new())
    }
}

/// `phutil_escape_html()` of a possibly `null` value.
pub fn escape(value: Option<&Markup>) -> Vec<u8> {
    value.map(Markup::escape).unwrap_or_default()
}

/// `phutil_escape_html()` of a list: each item escaped, then concatenated.
pub fn concat<'a>(parts: impl IntoIterator<Item = Option<&'a Markup>>) -> Markup {
    let mut output = Vec::new();
    for part in parts {
        output.extend_from_slice(&escape(part));
    }
    Markup::Html(output)
}

/// `phutil_implode_html($glue, $pieces)`.
pub fn implode_html<'a>(
    glue: &Markup,
    pieces: impl IntoIterator<Item = Option<&'a Markup>>,
) -> Markup {
    let glue = glue.escape();
    let mut output = Vec::new();
    for (index, piece) in pieces.into_iter().enumerate() {
        if index > 0 {
            output.extend_from_slice(&glue);
        }
        output.extend_from_slice(&escape(piece));
    }
    Markup::Html(output).into_safe()
}

const SELF_CLOSING: &[&str] = &[
    "area", "base", "br", "col", "command", "embed", "frame", "hr", "img", "input", "keygen",
    "link", "meta", "param", "source", "track", "wbr",
];

/// The content of a tag: `null`, one value, or a list of values.
#[derive(Debug, Clone, Copy)]
pub enum Content<'a> {
    /// `null`: self-closing tags close themselves.
    None,
    /// One value.
    One(&'a Markup),
    /// A list of possibly `null` values.
    List(&'a [Option<Markup>]),
}

/// `phutil_tag()`. Attributes with a `None` value are left out; values are
/// escaped as text (Remarkup only passes flat text there).
///
/// # Panics
///
/// If `href` is a `javascript:` URI, like PHP throws.
pub fn tag(name: &str, attributes: &[(&str, Option<&[u8]>)], content: Content<'_>) -> Markup {
    let mut output = Vec::new();
    output.push(b'<');
    output.extend_from_slice(name.as_bytes());
    for (key, value) in attributes {
        let Some(value) = value else {
            continue;
        };
        if *key == "href" {
            assert!(
                !is_javascript_uri(value),
                "javascript: URIs are never rendered"
            );
        }
        output.push(b' ');
        output.extend_from_slice(key.as_bytes());
        output.extend_from_slice(b"=\"");
        output.extend_from_slice(&php::htmlspecialchars(value));
        output.push(b'"');
    }
    let content = match content {
        Content::None if SELF_CLOSING.contains(&name) => {
            output.extend_from_slice(b" />");
            return Markup::Html(output);
        }
        Content::None => Vec::new(),
        Content::One(value) => value.escape(),
        Content::List(values) => concat(values.iter().map(Option::as_ref)).into_bytes(),
    };
    output.push(b'>');
    output.extend_from_slice(&content);
    output.extend_from_slice(b"</");
    output.extend_from_slice(name.as_bytes());
    output.push(b'>');
    Markup::Html(output)
}

/// Whether `phutil_tag()` refuses this `href`: after dropping everything
/// but letters, digits, `/` and `:`, it starts with `javascript:`.
fn is_javascript_uri(href: &[u8]) -> bool {
    if href.is_empty() || href[0] == b'/' || href[0] == b'#' {
        return false;
    }
    let normalized: Vec<u8> = href
        .iter()
        .copied()
        .filter(|byte| byte.is_ascii_alphanumeric() || *byte == b'/' || *byte == b':')
        .collect();
    normalized.len() >= 11 && normalized[..11].eq_ignore_ascii_case(b"javascript:")
}

/// `phutil_escape_html_newlines()`: `nl2br()` on the escaped value.
pub fn escape_newlines(value: &Markup) -> Markup {
    let escaped = value.escape();
    let mut output = Vec::with_capacity(escaped.len());
    let mut index = 0;
    while index < escaped.len() {
        let byte = escaped[index];
        let pair = match (byte, escaped.get(index + 1)) {
            (b'\r', Some(b'\n')) | (b'\n', Some(b'\r')) => true,
            (b'\r' | b'\n', _) => false,
            _ => {
                output.push(byte);
                index += 1;
                continue;
            }
        };
        output.extend_from_slice(b"<br />");
        let length = if pair { 2 } else { 1 };
        output.extend_from_slice(&escaped[index..index + length]);
        index += length;
    }
    Markup::Html(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_escape_text_but_not_html() {
        let text = Markup::text("<b>");
        let html = Markup::html("<b>x</b>");
        assert_eq!(
            tag("p", &[("class", Some(b"a\"b"))], Content::One(&text)).as_bytes(),
            b"<p class=\"a&quot;b\">&lt;b&gt;</p>"
        );
        assert_eq!(
            tag("p", &[], Content::One(&html)).as_bytes(),
            b"<p><b>x</b></p>"
        );
        assert_eq!(tag("br", &[], Content::None).as_bytes(), b"<br />");
        assert_eq!(
            tag("div", &[("x", None)], Content::None).as_bytes(),
            b"<div></div>"
        );
    }

    #[test]
    #[should_panic(expected = "javascript")]
    fn javascript_uris_are_refused() {
        let _ = tag(
            "a",
            &[("href", Some(b" java\nscript:alert(1)"))],
            Content::None,
        );
    }

    #[test]
    fn newlines_become_breaks() {
        assert_eq!(
            escape_newlines(&Markup::text("a\nb\r\nc<")).as_bytes(),
            b"a<br />\nb<br />\r\nc&lt;"
        );
    }
}
