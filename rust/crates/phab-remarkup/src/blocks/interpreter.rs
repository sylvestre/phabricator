//! `PhutilRemarkupInterpreterBlockRule`: `name (options) {{{ ... }}}`
//! blocks handed to an interpreter (`PhutilRemarkupBlockInterpreter`).

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::options::{self, OptionValue};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static START: Pattern = Pattern::new(r"^([\w]+)\s*(?:\(([^)]+)\)\s*)?{{{");
static END: Pattern = Pattern::new(r"}}}\s*$");

/// `PhutilRemarkupBlockInterpreter`.
pub trait Interpreter: Send + Sync {
    /// `getInterpreterName()`.
    fn name(&self) -> &'static str;

    /// `markupContent()`.
    fn markup_content(
        &self,
        cx: &mut Context<'_>,
        content: &[u8],
        options: &indexmap::IndexMap<Vec<u8>, OptionValue>,
    ) -> Markup;
}

/// `PhutilRemarkupInterpreterBlockRule`, with the interpreters it knows.
#[derive(Default)]
pub struct InterpreterBlock {
    interpreters: Vec<Box<dyn Interpreter>>,
}

impl InterpreterBlock {
    /// A rule handing blocks to these interpreters.
    pub fn new(interpreters: Vec<Box<dyn Interpreter>>) -> Self {
        Self { interpreters }
    }
}

impl BlockRule for InterpreterBlock {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupInterpreterBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if !preg::is_match(&START, lines[cursor]) {
            return 0;
        }
        // The line with the closing braces is not counted: PHP stops on it
        // before incrementing.
        1 + lines[cursor..]
            .iter()
            .take_while(|line| !preg::is_match(&END, line))
            .count()
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let mut lines: Vec<Vec<u8>> = php::explode_lines(text)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();
        while lines.len() > 1 && php::trim(lines.last().expect("not empty")).is_empty() {
            lines.pop();
        }
        let groups = preg::captures(&START, &lines[0])?;
        let name = groups.get(1).to_vec();
        let options = if groups.is_set(2) {
            options::parse(groups.get(2))
        } else {
            indexmap::IndexMap::new()
        };

        let last = lines.len() - 1;
        lines[0] = preg::replace(&START, &lines[0], b"");
        lines[last] = preg::replace(&END, &lines[last], b"");
        let mut kept: Vec<&[u8]> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let removable = index == 0 || index == last;
            if removable && php::trim(line).is_empty() {
                continue;
            }
            kept.push(line);
        }
        let content = php::implode(b"\n", &kept);

        if let Some(interpreter) = self
            .interpreters
            .iter()
            .find(|interpreter| interpreter.name().as_bytes() == name.as_slice())
        {
            return Some(interpreter.markup_content(cx, &content, &options));
        }

        let mut message = b"No interpreter found: ".to_vec();
        message.extend_from_slice(&name);
        if cx.is_text_mode() {
            let mut output = b"(".to_vec();
            output.extend_from_slice(&message);
            output.push(b')');
            return Some(Markup::Text(output));
        }
        Some(markup::tag(
            "div",
            &[("class", Some(b"remarkup-interpreter-error"))],
            Content::One(&Markup::Text(message)),
        ))
    }
}
