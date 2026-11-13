//! `PhutilRemarkupHeaderBlockRule`: `= Header =`, `## Header` and
//! underlined headers.

use crate::blocks::rule::blank_lines_after;
use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;
use crate::rules::anchor;

static ONE_LINE: Pattern = Pattern::new(r"^(={1,5}|#{2,5}|# ).*+$");
static UNDERLINED: Pattern = Pattern::new(r"^([^\n]+)\n[-=]{2,}\s*$");
static HANGING_WORD: Pattern =
    Pattern::new(r"-(a|an|the|at|by|for|in|of|on|per|to|up|and|as|but|if|or|nor)\z");

/// `PhutilRemarkupHeaderBlockRule`.
pub struct Header;

impl BlockRule for Header {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupHeaderBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        let num_lines = if preg::is_match(&ONE_LINE, lines[cursor]) {
            1
        } else if cursor + 1 < lines.len()
            && preg::is_match(&UNDERLINED, &[lines[cursor], lines[cursor + 1]].concat())
        {
            2
        } else {
            0
        };
        if num_lines == 0 {
            return 0;
        }
        num_lines + blank_lines_after(lines, cursor + num_lines)
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let text = php::trim(text);
        let lines = php::split_lines(text);
        let (level, text) = if lines.len() > 1 {
            let level = if lines[1].first() == Some(&b'=') {
                1
            } else {
                2
            };
            (level, php::trim(lines[0]))
        } else {
            let level = text
                .iter()
                .take(5)
                .take_while(|byte| **byte == b'=' || **byte == b'#')
                .count();
            (level, php::trim_chars(text, b" =#"))
        };

        if cx.is_text_mode() {
            let underline = if level == 1 { b'=' } else { b'-' };
            let mut output = text.to_vec();
            output.push(b'\n');
            output.extend(std::iter::repeat_n(underline, php::utf8_strlen(text)));
            return Some(Markup::Text(output));
        }

        let anchor = if cx.config().generate_toc {
            generate_anchor(cx, level, text)
        } else {
            None
        };
        let content = vec![anchor, Some(cx.apply_rules(Markup::text(text)))];
        let name = format!("h{}", level + 1);
        Some(markup::tag(
            &name,
            &[("class", Some(b"remarkup-header"))],
            Content::List(&content),
        ))
    }
}

/// `generateAnchor()`: records the header for the table of contents and
/// returns its `<a name>`.
fn generate_anchor(cx: &mut Context<'_>, level: usize, text: &[u8]) -> Option<Markup> {
    // Links in headers render as their names in the table of contents.
    cx.push_state("toc");
    let plain = cx.apply_rules(Markup::text(text));
    let plain = cx.restore_text(&plain).into_bytes();
    cx.pop_state("toc");

    let base = anchor_name_from_header_text(&plain);
    if base.is_empty() {
        return None;
    }
    let mut name = base.clone();
    let mut suffix = 1;
    while cx
        .metadata
        .toc
        .iter()
        .any(|(existing, _, _)| *existing == name)
    {
        let mut candidate = base.clone();
        candidate.push(b'-');
        candidate.extend_from_slice(suffix.to_string().as_bytes());
        name = php::trim_chars(&candidate, b"-").to_vec();
        suffix += 1;
    }
    cx.metadata.toc.push((name.clone(), level, plain));
    Some(markup::tag(
        "a",
        &[("name", Some(&name))],
        Content::One(&Markup::default()),
    ))
}

/// `getAnchorNameFromHeaderText()`.
pub fn anchor_name_from_header_text(text: &[u8]) -> Vec<u8> {
    let anchor = anchor::normalize_anchor(&php::utf8_strtolower(text));
    let anchor = truncate_glyphs(&anchor, 32);
    preg::replace(&HANGING_WORD, &anchor, b"")
}

/// `PhutilUTF8StringTruncator` with no terminator: at most `glyphs`
/// characters, combining marks staying with their base.
fn truncate_glyphs(text: &[u8], glyphs: usize) -> Vec<u8> {
    let text = String::from_utf8_lossy(text);
    let mut count = 0;
    let mut end = text.len();
    for (index, character) in text.char_indices() {
        if !is_combining(character) {
            if count == glyphs {
                end = index;
                break;
            }
            count += 1;
        }
    }
    text[..end].as_bytes().to_vec()
}

/// `phutil_utf8_is_combining_character()`.
fn is_combining(character: char) -> bool {
    matches!(u32::from(character), 0x0300..=0x036F | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// `renderTableOfContents()`: a nested list of the headers, if there are
/// at least two.
pub fn render_table_of_contents(toc: &[(Vec<u8>, usize, Vec<u8>)]) -> Option<Markup> {
    if toc.len() < 2 {
        return None;
    }
    let mut depth = 0;
    let mut parts = Vec::new();
    for (anchor, level, name) in toc {
        while depth < *level {
            parts.push(Some(Markup::html("<ul>")));
            depth += 1;
        }
        while depth > *level {
            parts.push(Some(Markup::html("</ul>")));
            depth -= 1;
        }
        let mut href = b"#".to_vec();
        href.extend_from_slice(anchor);
        // The name was restored from the storage: it is HTML already.
        let link = markup::tag(
            "a",
            &[("href", Some(&href))],
            Content::One(&Markup::html(name.clone())),
        );
        parts.push(Some(markup::tag("li", &[], Content::One(&link))));
    }
    while depth > 0 {
        parts.push(Some(Markup::html("</ul>")));
        depth -= 1;
    }
    Some(markup::implode_html(
        &Markup::text("\n"),
        parts.iter().map(Option::as_ref),
    ))
}
