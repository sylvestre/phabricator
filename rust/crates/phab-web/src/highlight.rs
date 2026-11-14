//! Syntax highlighting of the code in diffs, with Sublime Text grammars
//! (`syntect`) where PHP runs Pygments (`PhabricatorSyntaxHighlighter`).
//!
//! The scopes of a grammar are folded into a few classes the stylesheet
//! colours, so the output stays small and doesn't depend on a theme.

use std::ops::Range;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// `DifferentialChangesetParser::HIGHLIGHT_BYTE_LIMIT`: a side larger than
/// this is shown as plain text.
pub const BYTE_LIMIT: usize = 262_144;

/// A highlighted run of a line: its bytes and its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The bytes of the line the token covers.
    pub range: Range<usize>,
    /// The class of the stylesheet, like `hl-keyword`.
    pub class: &'static str,
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The scope prefixes, most specific first, and their classes.
fn classes() -> &'static [(Scope, &'static str)] {
    static CLASSES: OnceLock<Vec<(Scope, &'static str)>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        [
            ("comment", "hl-comment"),
            ("string", "hl-string"),
            ("constant.numeric", "hl-number"),
            ("constant.character.escape", "hl-string"),
            ("constant", "hl-constant"),
            ("keyword.operator", "hl-operator"),
            ("keyword", "hl-keyword"),
            ("storage.type", "hl-type"),
            ("storage", "hl-keyword"),
            ("entity.name.function", "hl-function"),
            ("support.function", "hl-function"),
            ("entity.name.tag", "hl-tag"),
            ("entity.other.attribute-name", "hl-attribute"),
            ("entity.name", "hl-type"),
            ("support.type", "hl-type"),
            ("support.class", "hl-type"),
            ("variable.parameter", "hl-variable"),
            ("markup.heading", "hl-keyword"),
            ("markup.inserted", "hl-inserted"),
            ("markup.deleted", "hl-deleted"),
        ]
        .into_iter()
        .map(|(scope, class)| (Scope::new(scope).expect("valid scope"), class))
        .collect()
    })
}

/// The class of the innermost scope of `stack` which has one.
fn class_of(stack: &ScopeStack) -> Option<&'static str> {
    stack.as_slice().iter().rev().find_map(|scope| {
        classes()
            .iter()
            .find(|(prefix, _)| prefix.is_prefix_of(*scope))
            .map(|(_, class)| *class)
    })
}

/// The grammar for a file, from its name: its extension, or the name
/// itself (`Makefile`).
pub fn syntax_for(filename: &str) -> Option<&'static SyntaxReference> {
    let syntaxes = syntaxes();
    let name = filename.rsplit('/').next().unwrap_or(filename);
    let syntax = name
        .rsplit_once('.')
        .and_then(|(_, extension)| syntaxes.find_syntax_by_extension(extension))
        .or_else(|| syntaxes.find_syntax_by_extension(name))?;
    // Plain text has nothing to highlight.
    (syntax.name != "Plain Text").then_some(syntax)
}

/// Adds a run to the tokens of a line, merged with the previous one when
/// they touch and share a class; runs without a class are left out.
fn push(tokens: &mut Vec<Token>, range: Range<usize>, class: Option<&'static str>) {
    let Some(class) = class.filter(|_| !range.is_empty()) else {
        return;
    };
    match tokens.last_mut() {
        Some(last) if last.class == class && last.range.end == range.start => {
            last.range.end = range.end;
        }
        _ => tokens.push(Token { range, class }),
    }
}

/// Highlights consecutive lines of one side of a file, carrying the
/// grammar's state from line to line like PHP highlights the whole corpus:
/// the tokens of each line. Plain text (`None`) if the lines are larger
/// than [`BYTE_LIMIT`] or the grammar fails.
pub fn highlight_lines(syntax: &SyntaxReference, lines: &[&str]) -> Option<Vec<Vec<Token>>> {
    if lines.iter().map(|line| line.len()).sum::<usize>() > BYTE_LIMIT {
        return None;
    }
    let syntaxes = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut highlighted = Vec::with_capacity(lines.len());
    for line in lines {
        // The grammars expect lines with their ending.
        let owned;
        let text = if line.ends_with('\n') {
            *line
        } else {
            owned = format!("{line}\n");
            &owned
        };
        let operations = state.parse_line(text, syntaxes).ok()?;
        let mut tokens: Vec<Token> = Vec::new();
        let mut start = 0;
        for (offset, operation) in operations {
            push(&mut tokens, start..offset, class_of(&stack));
            stack.apply(&operation).ok()?;
            start = offset;
        }
        // Offsets past the line are on the ending added above.
        push(&mut tokens, start..text.len(), class_of(&stack));
        for token in &mut tokens {
            token.range.end = token.range.end.min(line.len());
        }
        tokens.retain(|token| !token.range.is_empty());
        highlighted.push(tokens);
    }
    Some(highlighted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes_of(line: &str, tokens: &[Token]) -> Vec<(String, &'static str)> {
        tokens
            .iter()
            .map(|token| (line[token.range.clone()].to_owned(), token.class))
            .collect()
    }

    #[test]
    fn grammars_come_from_the_file_name() {
        assert_eq!(syntax_for("src/a.rs").unwrap().name, "Rust");
        assert_eq!(syntax_for("dom/base/Element.cpp").unwrap().name, "C++");
        assert_eq!(syntax_for("python/mach/main.py").unwrap().name, "Python");
        assert_eq!(syntax_for("build/Makefile").unwrap().name, "Makefile");
        assert!(syntax_for("README").is_none());
        assert!(syntax_for("notes.txt").is_none());
    }

    #[test]
    fn state_carries_over_lines() {
        let lines = ["int x = 1; /* a\n", "still comment */ return 0;\n"];
        let tokens = highlight_lines(syntax_for("a.c").unwrap(), &lines).unwrap();
        let first = classes_of(lines[0], &tokens[0]);
        assert!(first.contains(&("int".to_owned(), "hl-type")), "{first:?}");
        assert!(first.contains(&("1".to_owned(), "hl-number")), "{first:?}");
        assert!(
            first.contains(&("/* a\n".to_owned(), "hl-comment")),
            "{first:?}"
        );
        let second = classes_of(lines[1], &tokens[1]);
        assert_eq!(second[0], ("still comment */".to_owned(), "hl-comment"));
        assert!(
            second.contains(&("return".to_owned(), "hl-keyword")),
            "{second:?}"
        );
    }

    #[test]
    fn lines_without_endings_and_large_sides() {
        let tokens = highlight_lines(syntax_for("a.py").unwrap(), &["x = 'é'"]).unwrap();
        let tokens = &tokens[0];
        assert!(
            tokens
                .iter()
                .all(|token| token.range.end <= "x = 'é'".len())
        );
        assert!(classes_of("x = 'é'", tokens).contains(&("'é'".to_owned(), "hl-string")));

        let line = "x = 1\n".repeat(1000);
        let lines: Vec<&str> = std::iter::repeat_n(line.as_str(), 50).collect();
        assert!(highlight_lines(syntax_for("a.py").unwrap(), &lines).is_none());
    }
}
