//! `PhutilLexer`: a regex lexer with a stack of states. Each rule is a
//! pattern anchored at the current position (`\G`), a token type and an
//! optional state to push or pop.

use std::collections::HashMap;

use pcre2::bytes::Regex;

use crate::preg;

/// What a rule does to the state stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Stay in the current state.
    Stay,
    /// Push a state.
    Push(&'static str),
    /// Pop back to the previous state (`!pop`).
    Pop,
}

/// One rule of a state.
pub struct LexerRule {
    regex: Regex,
    token: &'static str,
    next: Next,
}

/// A lexer: the rules of each state.
pub struct Lexer {
    states: HashMap<&'static str, Vec<LexerRule>>,
}

/// The rules of one state: patterns, token types and transitions.
pub type StateRules = (&'static str, &'static [(&'static str, &'static str, Next)]);

/// A token: its type and text.
pub type Token = (&'static str, Vec<u8>);

impl Lexer {
    /// A lexer from `(state, [(pattern, token, next)])`, as
    /// `getRawRules()` lists them. Patterns get `\G` and PHP's `s` flag.
    pub fn new(rules: &[StateRules]) -> Self {
        let states = rules
            .iter()
            .map(|(state, rules)| {
                let rules = rules
                    .iter()
                    .map(|(pattern, token, next)| LexerRule {
                        regex: preg::compile(&format!("(?s)\\G(?:{pattern})")),
                        token,
                        next: *next,
                    })
                    .collect();
                (*state, rules)
            })
            .collect();
        Self { states }
    }

    /// `getTokens()`.
    ///
    /// # Panics
    ///
    /// Where PHP throws: no rule matches, an empty match without a state
    /// change, or popping the last state. Lexers of this crate always match.
    pub fn tokens(&self, input: &[u8]) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut states = vec!["start"];
        let mut position = 0;
        while position < input.len() {
            let state = states.last().expect("the start state is never popped");
            let rules = self.states.get(state).map_or(&[][..], Vec::as_slice);
            let (rule, length) = rules
                .iter()
                .find_map(|rule| {
                    let found = rule.regex.find_at(input, position).ok().flatten()?;
                    (found.start() == position).then(|| (rule, found.end() - position))
                })
                .unwrap_or_else(|| panic!("no lexer rule matched input at char {position}"));
            if length == 0 {
                assert!(rule.next != Next::Stay, "a rule matched an empty token");
            } else {
                tokens.push((rule.token, input[position..position + length].to_vec()));
                position += length;
            }
            match rule.next {
                Next::Stay => {}
                Next::Push(state) => states.push(state),
                Next::Pop => {
                    states.pop();
                    assert!(!states.is_empty(), "a rule popped the last state");
                }
            }
        }
        tokens
    }

    /// `mergeTokens()`: joins consecutive tokens of the same type.
    pub fn merge_tokens(tokens: Vec<Token>) -> Vec<Token> {
        let mut merged: Vec<Token> = Vec::new();
        for token in tokens {
            match merged.last_mut() {
                Some(last) if last.0 == token.0 => last.1.extend_from_slice(&token.1),
                _ => merged.push(token),
            }
        }
        merged
    }
}
