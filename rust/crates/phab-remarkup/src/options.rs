//! `PhutilSimpleOptions`: parses `key=value, flag` lists like
//! `lang=php, name=x.php`.

use std::sync::LazyLock;

use indexmap::IndexMap;

use crate::lexer::{Lexer, Next, Token};

static LEXER: LazyLock<Lexer> = LazyLock::new(|| {
    Lexer::new(&[
        (
            "start",
            &[
                (r"\s+", " ", Next::Stay),
                ("'", "'", Next::Push("string1")),
                ("\"", "\"", Next::Push("string2")),
                (",", ",", Next::Stay),
                ("=", "=", Next::Stay),
                (r#"[^\s'"=,]+"#, "word", Next::Stay),
            ],
        ),
        (
            "string1",
            &[
                (r"[^'\\]+", "word", Next::Stay),
                ("'", "'", Next::Pop),
                (r"\\.", "esc", Next::Stay),
                (r"\\$", "!pop", Next::Stay),
            ],
        ),
        (
            "string2",
            &[
                (r#"[^"\\]+"#, "word", Next::Stay),
                ("\"", "\"", Next::Pop),
                (r"\\.", "esc", Next::Stay),
                (r"\\$", "!pop", Next::Stay),
            ],
        ),
    ])
});

/// A parsed option: a value, or `true` for a bare key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionValue {
    /// `key` alone.
    Flag,
    /// `key=value`.
    Value(Vec<u8>),
}

impl OptionValue {
    /// PHP truthiness: `true`, or a string other than `""` and `"0"`.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Flag => true,
            Self::Value(bytes) => !bytes.is_empty() && bytes != b"0",
        }
    }

    /// `(string)` of the value: `true` is `"1"`.
    pub fn to_php_string(&self) -> Vec<u8> {
        match self {
            Self::Flag => b"1".to_vec(),
            Self::Value(bytes) => bytes.clone(),
        }
    }
}

/// `PhutilSimpleOptionsLexer::getNiceTokens()`.
fn nice_tokens(input: &[u8]) -> Vec<Token> {
    let tokens = LEXER
        .tokens(input)
        .into_iter()
        .map(|(kind, value)| match kind {
            "esc" => ("word", value[1..].to_vec()),
            _ => (kind, value),
        })
        .collect();
    let mut tokens = Lexer::merge_tokens(tokens);
    // A space between two words is part of a value.
    for index in 1..tokens.len().saturating_sub(1) {
        if tokens[index].0 == " " && tokens[index - 1].0 == "word" && tokens[index + 1].0 == "word"
        {
            tokens[index].0 = "word";
        }
    }
    tokens
        .into_iter()
        .filter(|(kind, _)| !matches!(*kind, "'" | "\"" | " "))
        .collect()
}

/// `PhutilSimpleOptions::parse()`, case-insensitive. An input which is not
/// an option list gives an empty map.
pub fn parse(input: &[u8]) -> IndexMap<Vec<u8>, OptionValue> {
    #[derive(PartialEq)]
    enum State {
        Key,
        Equals,
        Value,
        Comma,
    }
    let mut state = State::Key;
    let mut key = Vec::new();
    let mut pairs: Vec<(Vec<u8>, Option<OptionValue>)> = Vec::new();
    for (kind, value) in nice_tokens(input) {
        match state {
            State::Key => {
                if kind != "word" || value.is_empty() {
                    return IndexMap::new();
                }
                key = value.to_ascii_lowercase();
                state = State::Equals;
            }
            State::Equals => match kind {
                "=" => state = State::Value,
                "," => {
                    pairs.push((key.clone(), Some(OptionValue::Flag)));
                    state = State::Key;
                }
                _ => return IndexMap::new(),
            },
            State::Value => match kind {
                "," => {
                    pairs.push((key.clone(), None));
                    state = State::Key;
                }
                "word" => {
                    pairs.push((key.clone(), Some(OptionValue::Value(value))));
                    state = State::Comma;
                }
                _ => return IndexMap::new(),
            },
            State::Comma => match kind {
                "word" => {
                    if let Some((_, pair)) = pairs.last_mut() {
                        // PHP appends to `true` as the string "1".
                        let mut joined = match pair.take() {
                            Some(OptionValue::Value(bytes)) => bytes,
                            Some(OptionValue::Flag) => b"1".to_vec(),
                            None => Vec::new(),
                        };
                        joined.extend_from_slice(&value);
                        *pair = Some(OptionValue::Value(joined));
                    }
                }
                "," => state = State::Key,
                _ => return IndexMap::new(),
            },
        }
    }
    match state {
        State::Equals => pairs.push((key, Some(OptionValue::Flag))),
        State::Value => pairs.push((key, None)),
        State::Key | State::Comma => {}
    }

    let mut result = IndexMap::new();
    for (key, value) in pairs {
        match value {
            None => {
                result.shift_remove(&key);
            }
            Some(value) => {
                // A key set again keeps its first position, like PHP arrays.
                result.insert(key, value);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn parsed(input: &str) -> Vec<(String, Option<String>)> {
        parse(input.as_bytes())
            .into_iter()
            .map(|(key, value)| {
                (
                    String::from_utf8(key).unwrap(),
                    match value {
                        OptionValue::Flag => None,
                        OptionValue::Value(value) => Some(String::from_utf8(value).unwrap()),
                    },
                )
            })
            .collect()
    }

    // Cases from PhutilSimpleOptionsTestCase.
    #[rstest]
    #[case("", vec![])]
    #[case("legs=4", vec![("legs", Some("4"))])]
    #[case("legs=4,LEGS=4", vec![("legs", Some("4"))])]
    #[case("legs=4, legs=4", vec![("legs", Some("4"))])]
    #[case("a,b,c", vec![("a", None), ("b", None), ("c", None)])]
    #[case("a,b,c,d=", vec![("a", None), ("b", None), ("c", None)])]
    #[case("a=1,b=2", vec![("a", Some("1")), ("b", Some("2"))])]
    #[case("a='x\\'y'", vec![("a", Some("x'y"))])]
    #[case("name=a b", vec![("name", Some("a b"))])]
    #[case("lang=php, name=x.php", vec![("lang", Some("php")), ("name", Some("x.php"))])]
    #[case("Hello world", vec![])]
    #[case("x=y=z", vec![])]
    fn parses_like_php(#[case] input: &str, #[case] expected: Vec<(&str, Option<&str>)>) {
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.map(str::to_owned)))
            .collect();
        assert_eq!(parsed(input), expected);
    }
}
