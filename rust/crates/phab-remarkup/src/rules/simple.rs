//! The emphasis rules: `**bold**`, `//italic//`, `~~deleted~~`,
//! `__underlined__` and `!!highlighted!!`.

use crate::engine::Context;
use crate::markup::Markup;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static BOLD: Pattern = Pattern::new(r"(?s)\*\*(.+?)\*\*");
static ITALIC: Pattern = Pattern::new(r"(?s)(?<!:)//(.+?)//");
static DEL: Pattern = Pattern::new(r"(?s)(?<!~)~~([^\s~].*?~*)~~");
static UNDERLINE: Pattern = Pattern::new(r"(?s)(?<!_|/)__([^\s_/].*?_*)__(?!/|\.\S)");
static HIGHLIGHT: Pattern = Pattern::new(r"!!(.+?)(!{2,})");
static ONLY_BANGS: Pattern = Pattern::new(r"^!+\z");

/// `PhutilRemarkupBoldRule`.
pub struct Bold;

impl InlineRule for Bold {
    fn priority(&self) -> f64 {
        1000.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if cx.is_text_mode() {
            return text;
        }
        let anchor_mode = cx.is_anchor_mode();
        rule::replace_html(&BOLD, &text, |groups| {
            let inner = Markup::html(groups.get(1));
            if anchor_mode {
                inner
            } else {
                rule::hsprintf("<strong>%s</strong>", &[&inner])
            }
        })
    }
}

/// `PhutilRemarkupItalicRule`.
pub struct Italic;

impl InlineRule for Italic {
    fn priority(&self) -> f64 {
        1000.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if cx.is_text_mode() {
            return text;
        }
        rule::replace_html(&ITALIC, &text, |groups| {
            rule::hsprintf("<em>%s</em>", &[&Markup::html(groups.get(1))])
        })
    }
}

/// `PhutilRemarkupDelRule`.
pub struct Del;

impl InlineRule for Del {
    fn priority(&self) -> f64 {
        1000.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if cx.is_text_mode() {
            return text;
        }
        rule::replace_html(&DEL, &text, |groups| {
            rule::hsprintf("<del>%s</del>", &[&Markup::html(groups.get(1))])
        })
    }
}

/// `PhutilRemarkupUnderlineRule`.
pub struct Underline;

impl InlineRule for Underline {
    fn priority(&self) -> f64 {
        1000.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if cx.is_text_mode() {
            return text;
        }
        rule::replace_html(&UNDERLINE, &text, |groups| {
            rule::hsprintf("<u>%s</u>", &[&Markup::html(groups.get(1))])
        })
    }
}

/// `PhutilRemarkupHighlightRule`.
pub struct Highlight;

impl InlineRule for Highlight {
    fn priority(&self) -> f64 {
        1000.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        if cx.is_text_mode() {
            return text;
        }
        rule::replace_html(&HIGHLIGHT, &text, |groups| {
            // "!!!!!" stays five exclamation points.
            if preg::is_match(&ONLY_BANGS, groups.get(1)) {
                return Markup::html(groups.get(0));
            }
            // The two exclamation points of the syntax go away.
            let excitement = Markup::text(&groups.get(2)[2..]);
            rule::hsprintf(
                "<span class=\"remarkup-highlight\">%s%s</span>",
                &[&Markup::html(groups.get(1)), &excitement],
            )
        })
    }
}
