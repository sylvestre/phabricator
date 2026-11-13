//! `PhutilRemarkupQuotedBlockRule` and its two rules: `> quoted text` and
//! `>>! In D123, alice wrote:` replies.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::rule::BlockRule;

/// `normalizeQuotedBody()`: removes the `>` and, if every line has one,
/// the space after it.
fn normalize_quoted_body(text: &[u8]) -> Vec<u8> {
    let mut lines: Vec<&[u8]> = php::split_lines(text)
        .into_iter()
        .map(|line| line.get(1..).unwrap_or_default())
        .collect();
    let strip_space = lines.iter().all(|line| match line.first() {
        None | Some(b'>' | b' ' | b'\n') => true,
        Some(_) => false,
    });
    if strip_space {
        for line in &mut lines {
            if line.first() == Some(&b' ') {
                *line = &line[1..];
            }
        }
    }
    let first = lines
        .iter()
        .position(|line| !php::is_blank(line))
        .unwrap_or(lines.len());
    lines[first..].concat()
}

/// `getQuotedText()`: the text mode rendering.
fn quoted_text(text: &[u8]) -> Vec<u8> {
    let text = php::rtrim(text, b"\n");
    php::split_lines(text)
        .into_iter()
        .flat_map(|line| {
            let prefix: &[u8] = match line.first() {
                Some(b'>' | b'\n') => b">",
                _ => b"> ",
            };
            [prefix, line].concat()
        })
        .collect()
}

fn will_quote(cx: &mut Context<'_>) {
    cx.set_quote_depth(cx.quote_depth() + 1);
}

fn did_quote(cx: &mut Context<'_>) {
    cx.set_quote_depth(cx.quote_depth().saturating_sub(1));
}

/// `PhutilRemarkupQuotesBlockRule`.
pub struct Quotes;

impl BlockRule for Quotes {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupQuotesBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        lines[cursor..]
            .iter()
            .take_while(|line| line.first() == Some(&b'>'))
            .count()
    }

    fn supports_child_blocks(&self) -> bool {
        true
    }

    fn extract_child_text(&self, text: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (Vec::new(), normalize_quoted_body(text))
    }

    fn will_markup_child_blocks(&self, cx: &mut Context<'_>) {
        will_quote(cx);
    }

    fn did_markup_child_blocks(&self, cx: &mut Context<'_>) {
        did_quote(cx);
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        _text: &[u8],
        children: Option<Markup>,
    ) -> Option<Markup> {
        let children = children.unwrap_or_default();
        if cx.is_text_mode() {
            return Some(Markup::Text(quoted_text(children.as_bytes())));
        }
        let style: Option<&[u8]> = cx.is_html_mail_mode().then_some(
            b"border-left: 3px solid #a7b5bf; color: #464c5c; font-style: italic; \
              margin: 4px 0 12px 0; padding: 4px 12px; background-color: #f8f9fc;",
        );
        Some(markup::tag(
            "blockquote",
            &[("style", style)],
            Content::One(&children),
        ))
    }
}

/// `PhutilRemarkupReplyBlockRule`.
pub struct Reply;

impl BlockRule for Reply {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupReplyBlockRule"
    }

    fn priority(&self) -> i32 {
        400
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if !lines[cursor].starts_with(b">>!") {
            return 0;
        }
        1 + lines[cursor + 1..]
            .iter()
            .take_while(|line| line.first() == Some(&b'>'))
            .count()
    }

    fn supports_child_blocks(&self) -> bool {
        true
    }

    fn extract_child_text(&self, text: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let lines = php::split_lines(text);
        let head = lines[0].get(3..).unwrap_or_default();
        let body = normalize_quoted_body(&lines[1..].concat());
        (php::trim(head).to_vec(), body)
    }

    fn will_markup_child_blocks(&self, cx: &mut Context<'_>) {
        will_quote(cx);
    }

    fn did_markup_child_blocks(&self, cx: &mut Context<'_>) {
        did_quote(cx);
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        children: Option<Markup>,
    ) -> Option<Markup> {
        let text = cx.apply_rules(Markup::text(text));
        let children = children.unwrap_or_default();
        if cx.is_text_mode() {
            let mut output = text.into_bytes();
            output.extend_from_slice(b"\n\n");
            output.extend_from_slice(&quoted_text(children.as_bytes()));
            return Some(Markup::Text(output));
        }
        let (block, head, body): (&[u8], &[u8], &[u8]) = if cx.is_html_mail_mode() {
            (
                b"border-left: 3px solid #8C98B8;\n          color: #6B748C;\n          font-style: italic;\n          margin: 4px 0 12px 0;\n          padding: 8px 12px;\n          background-color: #F8F9FC;",
                b"font-style: normal;\n          padding-bottom: 4px;",
                b"margin: 0;\n          padding: 0;\n          border: 0;\n          color: rgb(107, 116, 140);",
            )
        } else {
            (
                b"remarkup-reply-block",
                b"remarkup-reply-head",
                b"remarkup-reply-body",
            )
        };
        let key = if cx.is_html_mail_mode() {
            "style"
        } else {
            "class"
        };
        let newline = Markup::text("\n");
        let content = vec![
            Some(newline.clone()),
            Some(markup::tag(
                "div",
                &[(key, Some(head))],
                Content::One(&text),
            )),
            Some(newline.clone()),
            Some(markup::tag(
                "div",
                &[(key, Some(body))],
                Content::One(&children),
            )),
            Some(newline),
        ];
        Some(markup::tag(
            "blockquote",
            &[(key, Some(block))],
            Content::List(&content),
        ))
    }
}
