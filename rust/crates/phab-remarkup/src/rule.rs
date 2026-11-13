//! The two kinds of rules: block rules (`PhutilRemarkupBlockRule`), which
//! claim lines and render them, and inline rules (`PhutilRemarkupRule`),
//! which block rules apply to the text inside them.

use crate::engine::Context;
use crate::markup::Markup;
use crate::preg::{self, Groups, Pattern};
use crate::storage::MAGIC_BYTE;

/// `PhutilRemarkupBlockRule`.
pub trait BlockRule: Send + Sync {
    /// The PHP class name, which orders rules of the same priority.
    fn class_name(&self) -> &'static str;

    /// `getPriority()`: rules with smaller numbers try first.
    fn priority(&self) -> i32 {
        500
    }

    /// `getMatchingLineCount()`: how many lines from `cursor` this rule
    /// claims, or 0.
    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize;

    /// `markupText()`. `children` holds the rendered child blocks of rules
    /// which have some.
    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        children: Option<Markup>,
    ) -> Option<Markup>;

    /// `PhutilRemarkupDefaultBlockRule` blocks merge into paragraphs.
    fn is_default(&self) -> bool {
        false
    }

    /// `supportsChildBlocks()`.
    fn supports_child_blocks(&self) -> bool {
        false
    }

    /// `extractChildText()`: the block's own text and its children's.
    fn extract_child_text(&self, text: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (text.to_vec(), Vec::new())
    }

    /// `willMarkupChildBlocks()`.
    fn will_markup_child_blocks(&self, _cx: &mut Context<'_>) {}

    /// `didMarkupChildBlocks()`.
    fn did_markup_child_blocks(&self, _cx: &mut Context<'_>) {}

    /// `didMarkupText()`, once all text is marked up.
    fn postprocess(&self, _cx: &mut Context<'_>) {}
}

/// `PhutilRemarkupRule`.
pub trait InlineRule: Send + Sync {
    /// `getPriority()`: rules with smaller numbers apply first.
    fn priority(&self) -> f64 {
        500.0
    }

    /// `apply()`.
    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup;

    /// `didMarkupText()`, once all text is marked up.
    fn did_markup_text(&self, _cx: &mut Context<'_>) {}
}

/// `PhutilRemarkupRule::replaceHTML()`: matches on the escaped text, with
/// the groups and the result treated as HTML.
pub fn replace_html(
    pattern: &Pattern,
    text: &Markup,
    mut callback: impl FnMut(&Groups<'_>) -> Markup,
) -> Markup {
    let escaped = text.escape();
    let output = preg::replace_callback(pattern, &escaped, |groups| callback(groups).escape());
    Markup::Html(output).into_safe()
}

/// `PhutilRemarkupRule::isFlatText()`: the text holds no tokens.
pub fn is_flat_text(text: &[u8]) -> bool {
    !text.contains(&MAGIC_BYTE)
}

/// `hsprintf()` with `%s` arguments: the format is HTML, the arguments are
/// escaped unless they are HTML.
pub fn hsprintf(format: &str, arguments: &[&Markup]) -> Markup {
    let mut output = Vec::new();
    let mut arguments = arguments.iter();
    let mut pieces = format.split("%s");
    if let Some(first) = pieces.next() {
        output.extend_from_slice(first.as_bytes());
    }
    for piece in pieces {
        if let Some(argument) = arguments.next() {
            output.extend_from_slice(&argument.escape());
        }
        output.extend_from_slice(piece.as_bytes());
    }
    Markup::Html(output)
}
