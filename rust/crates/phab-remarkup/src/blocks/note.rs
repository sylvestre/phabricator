//! `PhutilRemarkupNoteBlockRule`: `NOTE:`, `(IMPORTANT)`, `WARNING:`.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static NOTE: Pattern = Pattern::new(
    r"^(?:(?:\((?P<hideword>NOTE|IMPORTANT|WARNING)\))|(?:(?P<showword>NOTE|IMPORTANT|WARNING):))\s*",
);

/// `PhutilRemarkupNoteBlockRule`.
pub struct Note;

impl BlockRule for Note {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupNoteBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if !preg::is_match(&NOTE, lines[cursor]) {
            return 0;
        }
        // PHP's `if (trim($line))`: "0" stops the note too.
        1 + lines[cursor + 1..]
            .iter()
            .take_while(|line| {
                let trimmed = php::trim(line);
                !trimmed.is_empty() && trimmed != b"0"
            })
            .count()
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let groups = preg::captures(&NOTE, text)?;
        // Groups: 1 hideword, 2 showword.
        let (word, show) = if groups.get(2).is_empty() {
            (groups.get(1), false)
        } else {
            (groups.get(2), true)
        };
        let class_suffix = php::utf8_strtolower(word);
        let matched = groups.get(0).len();
        let word_part = php::rtrim(&text[..matched], php::WHITESPACE);
        let text_part = cx.apply_rules(Markup::text(php::rtrim(&text[matched..], php::WHITESPACE)));

        if cx.is_text_mode() {
            let mut output = word_part.to_vec();
            output.push(b' ');
            output.extend_from_slice(text_part.as_bytes());
            return Some(Markup::Text(output));
        }

        let content = if show {
            let word = markup::tag(
                "span",
                &[("class", Some(b"remarkup-note-word"))],
                Content::One(&Markup::text(word_part)),
            );
            markup::concat([Some(&word), Some(&Markup::text(" ")), Some(&text_part)])
        } else {
            text_part
        };

        let (key, value): (&str, Option<Vec<u8>>) = if cx.is_html_mail_mode() {
            let style = match class_suffix.as_slice() {
                b"important" => Some(
                    "margin: 16px 0;\n            padding: 12px;\n            border-left: 3px solid #c0392b;\n            background: #f4dddb;",
                ),
                b"note" => Some(
                    "margin: 16px 0;\n            padding: 12px;\n            border-left: 3px solid #2980b9;\n            background: #daeaf3;",
                ),
                b"warning" => Some(
                    "margin: 16px 0;\n            padding: 12px;\n            border-left: 3px solid #f1c40f;\n            background: #fdf5d4;",
                ),
                _ => None,
            };
            ("style", style.map(|style| style.as_bytes().to_vec()))
        } else {
            let mut class = b"remarkup-".to_vec();
            class.extend_from_slice(&class_suffix);
            ("class", Some(class))
        };
        Some(markup::tag(
            "div",
            &[(key, value.as_deref())],
            Content::One(&content),
        ))
    }
}
