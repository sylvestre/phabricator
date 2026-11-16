//! `PhutilRemarkupEvalRule`: `${{{strings.platform.server.name}}}`
//! expands to a few product strings.

use crate::engine::Context;
use crate::markup::Markup;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static EXPRESSION: Pattern = Pattern::new(r"\$\{\{\{(.+?)\}\}\}");

/// `PhutilRemarkupEvalRule`.
pub struct Eval;

/// `evaluateExpression()`: the strings an expression may name; anything
/// else stays as written.
fn evaluate(expression: &[u8]) -> Option<&'static [u8]> {
    match expression {
        b"strings.platform.server.name" => Some(b"Phabricator"),
        b"strings.platform.server.path" => Some(b"phabricator/"),
        b"strings.platform.client.name" => Some(b"Arcanist"),
        b"strings.platform.client.path" => Some(b"arcanist/"),
        _ => None,
    }
}

impl InlineRule for Eval {
    fn priority(&self) -> f64 {
        50.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(
            &EXPRESSION,
            text.as_bytes(),
            |groups| {
                let expression = groups.get(1);
                if !rule::is_flat_text(expression) {
                    return groups.get(0).to_vec();
                }
                // PHP evaluates once all text is marked up; the result is the
                // same.
                let result = evaluate(expression).unwrap_or(groups.get(0));
                cx.store_text(Markup::text(result))
            },
        ))
    }
}
