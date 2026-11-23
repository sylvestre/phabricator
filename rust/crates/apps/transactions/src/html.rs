//! The HTML helpers of PHP's `render.php` the mail bodies use.

/// `phutil_escape_html()` of a string: `htmlspecialchars()` with
/// `ENT_QUOTES`.
pub(crate) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#039;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// `phutil_escape_html_newlines()`: `nl2br()`, which puts `<br />` before
/// each line break (`\r\n`, `\n\r`, `\n` or `\r`).
pub(crate) fn nl2br(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut characters = html.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' | '\n' => {
                result.push_str("<br />");
                result.push(character);
                let pair = if character == '\r' { '\n' } else { '\r' };
                if characters.peek() == Some(&pair) {
                    result.push(pair);
                    characters.next();
                }
            }
            other => result.push(other),
        }
    }
    result
}

/// `phutil_tag()` with safe content: attributes escaped, in order.
pub(crate) fn tag(name: &str, attributes: &[(&str, &str)], content: &str) -> String {
    let mut html = format!("<{name}");
    for (key, value) in attributes {
        html.push(' ');
        html.push_str(key);
        html.push_str("=\"");
        html.push_str(&escape(value));
        html.push('"');
    }
    html.push('>');
    html.push_str(content);
    html.push_str("</");
    html.push_str(name);
    html.push('>');
    html
}

/// `phutil_tag('br')`.
pub(crate) const BR: &str = "<br />";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_htmlspecialchars() {
        assert_eq!(escape("a<b>&\"c'"), "a&lt;b&gt;&amp;&quot;c&#039;");
    }

    #[test]
    fn breaks_lines_like_nl2br() {
        assert_eq!(
            nl2br("a\nb\r\nc\n\rd\re"),
            "a<br />\nb<br />\r\nc<br />\n\rd<br />\re"
        );
    }

    #[test]
    fn renders_tags() {
        assert_eq!(
            tag("a", &[("style", ""), ("href", "x\"y")], "z"),
            "<a style=\"\" href=\"x&quot;y\">z</a>"
        );
    }
}
