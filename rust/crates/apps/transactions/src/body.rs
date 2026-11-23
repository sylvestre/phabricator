//! `PhabricatorMetaMTAMailBody`: the text and HTML sections of a mail.

use crate::html::{self, BR};

/// PHP's `rtrim()` with its default characters.
pub(crate) fn rtrim(text: &str) -> &str {
    text.trim_end_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// A mail body being composed.
#[derive(Debug, Default)]
pub(crate) struct Body {
    sections: Vec<String>,
    html_sections: Vec<String>,
}

impl Body {
    /// `addRawPlaintextSection()`.
    pub(crate) fn add_raw_plaintext(&mut self, text: &str) {
        if !text.is_empty() {
            self.sections.push(rtrim(text).to_owned());
        }
    }

    /// `addRawHTMLSection()`.
    pub(crate) fn add_raw_html(&mut self, html: String) {
        self.html_sections.push(html);
    }

    /// `addPlaintextSection()`: a `null` header is an empty line.
    pub(crate) fn add_plaintext(&mut self, header: Option<&str>, text: &str, indent: bool) {
        let text = if indent {
            self::indent(text)
        } else {
            text.to_owned()
        };
        self.sections
            .push(format!("{}\n{text}", header.unwrap_or_default()));
    }

    /// `addHTMLSection()`.
    pub(crate) fn add_html(&mut self, header: Option<&str>, fragment: &str) {
        let header = header
            .map(|header| html::tag("strong", &[], &html::escape(header)))
            .unwrap_or_default();
        let inner = html::tag("div", &[], fragment);
        self.html_sections
            .push(html::tag("div", &[], &format!("{header}{inner}")));
    }

    /// `addTextSection()` with a string.
    pub(crate) fn add_text(&mut self, header: &str, text: &str) {
        self.add_plaintext(Some(header), text, true);
        let fragment = html::nl2br(&html::tag("div", &[], &html::escape(text)));
        self.add_html(Some(header), &fragment);
    }

    /// `addLinkSection()`.
    pub(crate) fn add_link(&mut self, header: &str, link: &str) {
        self.add_plaintext(Some(header), link, true);
        let anchor = html::tag("a", &[("href", link)], &html::escape(link));
        self.add_html(Some(header), &anchor);
    }

    /// `addRemarkupSection()`, given the text and HTML mail renderings.
    pub(crate) fn add_remarkup(&mut self, header: Option<&str>, text: &str, html: &str) {
        self.add_plaintext(header, text, true);
        self.add_html(header, html);
    }

    /// `render()`.
    pub(crate) fn render(&self) -> String {
        format!("{}\n", self.sections.join("\n\n"))
    }

    /// `renderHTML()`.
    pub(crate) fn render_html(&self) -> String {
        format!("{}{BR}", self.html_sections.join(BR))
    }
}

/// `indent()`: two spaces before each line, trailing whitespace removed.
fn indent(text: &str) -> String {
    rtrim(&format!("  {}", text.replace('\n', "\n  "))).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_sections_like_php() {
        let mut body = Body::default();
        body.add_raw_plaintext("alice added a comment.\n");
        body.add_raw_html("<table></table>".to_owned());
        body.add_remarkup(None, "Hello\nthere", "<p>Hello<br />\nthere</p>");
        body.add_text("REPOSITORY", "rX <x>");
        body.add_link("REVISION DETAIL", "https://phab/D1");
        assert_eq!(
            body.render(),
            "alice added a comment.\n\n\n  Hello\n  there\n\nREPOSITORY\n  rX <x>\n\n\
             REVISION DETAIL\n  https://phab/D1\n"
        );
        assert_eq!(
            body.render_html(),
            "<table></table><br /><div><div><p>Hello<br />\nthere</p></div></div><br />\
             <div><strong>REPOSITORY</strong><div><div>rX &lt;x&gt;</div></div></div><br />\
             <div><strong>REVISION DETAIL</strong><div><a href=\"https://phab/D1\">\
             https://phab/D1</a></div></div><br />"
        );
    }
}
