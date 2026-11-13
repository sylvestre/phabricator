//! Code block highlighting: `PhutilLanguageGuesser` and the highlighters
//! of `PhutilDefaultSyntaxHighlighterEngine` that need no external tool.
//!
//! Languages PHP highlights with its lexers or Pygments are shown as plain
//! text: the Rust interface will highlight them itself (syntect), and the
//! code is the same either way.

use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule;

static GUESSES: [(Pattern, Guess); 7] = [
    (Pattern::new(r"^#!.*bin/env\s+(\S+)"), Guess::Group),
    (Pattern::new(r"^#!.*bin/(\S+)"), Guess::Group),
    (Pattern::new(r"^\s*<[?](php)"), Guess::Group),
    (
        Pattern::new(r"(?m)^.*-[*]-.*mode\s*:\s*(\S+).*-[*]-.*$"),
        Guess::Group,
    ),
    (
        Pattern::new(r"(?m)^---.*$\n^[+]{3}.*$\n^@@"),
        Guess::Language("diff"),
    ),
    (Pattern::new(r"^diff --git"), Guess::Language("diff")),
    (
        Pattern::new(r"^(?:\S+[\\/] )?[$] "),
        Guess::Language("console"),
    ),
];

enum Guess {
    Group,
    Language(&'static str),
}

/// `PhutilLanguageGuesser::guessLanguage()`.
pub fn guess_language(source: &[u8]) -> Option<Vec<u8>> {
    GUESSES.iter().find_map(|(pattern, guess)| {
        let groups = preg::captures(pattern, source)?;
        Some(match guess {
            Guess::Group => groups.get(1).to_vec(),
            Guess::Language(language) => language.as_bytes().to_vec(),
        })
    })
}

/// `PhutilDefaultSyntaxHighlighterEngine::highlightSource()`.
pub fn highlight(language: &[u8], source: &[u8]) -> Markup {
    match language {
        b"console" => console(source),
        b"invisible" => invisible(source),
        _ => Markup::text(source).into_escaped(),
    }
}

/// `PhutilConsoleSyntaxHighlighter`: prompts, commands and comments.
fn console(source: &[u8]) -> Markup {
    static START: Pattern = Pattern::new(r"^(\S+[\\/] )?([$] .*?)(#.*|\\)?$");
    static CONTINUED: Pattern = Pattern::new(r"()(.*?)(#.*|\\)?$");
    let mut in_command = false;
    let lines: Vec<Option<Markup>> = source
        .split(|byte| *byte == b'\n')
        .map(|line| {
            let pattern = if in_command { &CONTINUED } else { &START };
            Some(match preg::captures(pattern, line) {
                Some(groups) => {
                    let comment = groups.get(3);
                    let comment = if comment.is_empty() || comment == b"0" {
                        Markup::default()
                    } else {
                        rule::hsprintf("<span class=\"k\">%s</span>", &[&Markup::text(comment)])
                    };
                    in_command = comment_is_continuation(groups.get(3));
                    rule::hsprintf(
                        "%s<span class=\"gp\">%s</span>%s",
                        &[
                            &Markup::text(groups.get(1)),
                            &Markup::text(groups.get(2)),
                            &comment,
                        ],
                    )
                }
                None => rule::hsprintf("<span class=\"go\">%s</span>", &[&Markup::text(line)]),
            })
        })
        .collect();
    markup::implode_html(&Markup::text("\n"), lines.iter().map(Option::as_ref))
}

fn comment_is_continuation(comment: &[u8]) -> bool {
    comment == b"\\"
}

/// `PhutilInvisibleSyntaxHighlighter`: control characters as visible
/// symbols (U+2400 and on).
fn invisible(source: &[u8]) -> Markup {
    let mut parts: Vec<Option<Markup>> = Vec::new();
    for &byte in source {
        if byte < 0x20 {
            let symbol =
                char::from_u32(0x2400 + u32::from(byte)).expect("U+2400..U+241F are characters");
            let symbol = Markup::text(symbol.to_string());
            parts.push(Some(markup::tag(
                "span",
                &[("class", Some(b"invisible"))],
                Content::One(&symbol),
            )));
            if byte == b'\n' {
                parts.push(Some(Markup::text("\n")));
            }
        } else {
            parts.push(Some(Markup::text([byte])));
        }
    }
    markup::implode_html(&Markup::default(), parts.iter().map(Option::as_ref))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("#!/usr/bin/env python\nx", Some("python"))]
    #[case("#!/bin/sh\n", Some("sh"))]
    #[case("<?php\n", Some("php"))]
    #[case("--- a\n+++ b\n@@ x", Some("diff"))]
    #[case("$ ls", Some("console"))]
    #[case("hello", None)]
    fn guesses(#[case] source: &str, #[case] expected: Option<&str>) {
        assert_eq!(
            guess_language(source.as_bytes()),
            expected.map(|language| language.as_bytes().to_vec())
        );
    }

    #[test]
    fn console_marks_prompts() {
        assert_eq!(
            highlight(b"console", b"$ ls # list\nfile").as_bytes(),
            b"<span class=\"gp\">$ ls </span><span class=\"k\"># list</span>\n<span class=\"go\">file</span>"
        );
    }
}
