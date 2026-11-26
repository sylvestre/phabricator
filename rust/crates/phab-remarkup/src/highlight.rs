//! Code block highlighting: `PhutilLanguageGuesser` and the highlighters
//! of `PhutilDefaultSyntaxHighlighterEngine` that need no external tool.
//!
//! Since Bug 2093445 PHP highlights every language with Pygments (its
//! lexers for PHP, Python, Java and JSON, and its console highlighter, are
//! gone): those go to the engine's [`Highlighter`] (syntect for the pages),
//! or are shown as plain text.

use crate::engine::Highlighter;
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};

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
pub fn highlight(language: &[u8], source: &[u8], highlighter: Option<&Highlighter>) -> Markup {
    match language {
        b"invisible" => invisible(source),
        _ => highlighter
            .and_then(|highlighter| (highlighter.0)(language, source))
            .unwrap_or_else(|| Markup::text(source).into_escaped()),
    }
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
    fn console_is_plain_without_a_highlighter() {
        assert_eq!(
            highlight(b"console", b"$ ls # list\n<file>", None).as_bytes(),
            b"$ ls # list\n&lt;file&gt;"
        );
    }
}
