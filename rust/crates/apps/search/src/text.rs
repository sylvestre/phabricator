//! The text functions of the Ferret engine: case folding, term corpora
//! (`PhabricatorFerretEngine::newTermsCorpus()`), the stemmer
//! (`PhutilSearchStemmer`) and ngrams (`PhabricatorSearchNgramEngine`).
//!
//! Text outside ASCII is handled as PHP handles it, with one limit:
//! `mb_convert_case()` and Rust may lower some characters differently
//! (another Unicode version, conditional mappings), so text with a
//! character beyond ASCII which has a lowercase form is [`Unsupported`].

use crate::{php_sort, porter};

/// Text Rust can't process as PHP does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Unsupported(pub &'static str);

/// U+180E MONGOLIAN VOWEL SEPARATOR: `\s` of PCRE2's `UCP` mode matches it
/// in some versions only.
const MONGOLIAN_VOWEL_SEPARATOR: char = '\u{180e}';

/// `phutil_utf8_strtolower()`: `mb_convert_case($text, MB_CASE_LOWER)`.
pub fn lowercase(text: &str) -> Result<String, Unsupported> {
    let mut lowered = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii() {
            lowered.push(character.to_ascii_lowercase());
            continue;
        }
        let mut mapping = character.to_lowercase();
        if mapping.len() != 1 || mapping.next() != Some(character) {
            return Err(Unsupported("text with uppercase characters beyond ASCII"));
        }
        lowered.push(character);
    }
    Ok(lowered)
}

/// `\s` with PHP's `u` flag (PCRE2's `UTF` and `UCP`): Unicode white
/// space.
fn is_unicode_space(character: char) -> Result<bool, Unsupported> {
    if character == MONGOLIAN_VOWEL_SEPARATOR {
        return Err(Unsupported("text with U+180E"));
    }
    Ok(character.is_whitespace())
}

/// `preg_split('/\s+/u', $text)`: the pieces between runs of white space,
/// empty ones included.
fn split_unicode_space(text: &str) -> Result<Vec<&str>, Unsupported> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (index, character) in text.char_indices() {
        if is_unicode_space(character)? {
            if !in_space {
                pieces.push(&text[start..index]);
                in_space = true;
            }
        } else if in_space {
            start = index;
            in_space = false;
        }
    }
    pieces.push(if in_space { "" } else { &text[start..] });
    Ok(pieces)
}

/// `preg_replace('/\s+/u', ' ', $text)`.
fn collapse_unicode_space(text: &str) -> Result<String, Unsupported> {
    let mut output = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if is_unicode_space(character)? {
            if !in_space {
                output.push(' ');
                in_space = true;
            }
        } else {
            output.push(character);
            in_space = false;
        }
    }
    Ok(output)
}

/// The characters `newTermsCorpus()` turns into spaces with `strtr()`.
fn is_term_separator(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'"'
            | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b'-'
            | b'/'
            | b':'
            | b';'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'@'
            | b'['
            | b'\\'
            | b']'
            | b'^'
            | b'`'
            | b'{'
            | b'|'
            | b'}'
            | b'~'
            | b'.'
            | b'_'
            | b'\n'
            | b'\r'
            | b'\t'
    )
}

/// `preg_replace('/(^| )[\']+/', ' ', ...)` then
/// `preg_replace('/[\']+( |$)/', ' ', ...)`: quotes which start or end a
/// word divide terms, those inside a word (`whom'st've`) don't.
fn strip_quotes(text: &[u8]) -> Vec<u8> {
    // Leading quotes: a match starts at the start or at a space, which it
    // consumes, and replaces the quotes after it with one space.
    let mut leading = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let at_start = index == 0;
        let quotes_from = if at_start && text[0] == b'\'' {
            Some(0)
        } else if text[index] == b' ' && text.get(index + 1) == Some(&b'\'') {
            Some(index + 1)
        } else {
            None
        };
        if let Some(from) = quotes_from {
            let mut end = from;
            while text.get(end) == Some(&b'\'') {
                end += 1;
            }
            leading.push(b' ');
            index = end;
        } else {
            leading.push(text[index]);
            index += 1;
        }
    }

    // Trailing quotes: a run of quotes followed by a space, which the match
    // consumes, or by the end (there is no newline left for `$` to stop
    // before).
    let mut trailing = Vec::with_capacity(leading.len());
    let mut index = 0;
    while index < leading.len() {
        if leading[index] != b'\'' {
            trailing.push(leading[index]);
            index += 1;
            continue;
        }
        let mut end = index;
        while leading.get(end) == Some(&b'\'') {
            end += 1;
        }
        match leading.get(end) {
            Some(b' ') => {
                trailing.push(b' ');
                index = end + 1;
            }
            None => {
                trailing.push(b' ');
                index = end;
            }
            Some(_) => {
                trailing.extend_from_slice(&leading[index..end]);
                index = end;
            }
        }
    }
    trailing
}

/// `PhabricatorFerretEngine::newTermsCorpus()`: words separated by single
/// spaces, with a space before and after, or an empty string.
pub fn terms_corpus(raw: &str) -> Result<String, Unsupported> {
    let separated: Vec<u8> = raw
        .bytes()
        .map(|byte| if is_term_separator(byte) { b' ' } else { byte })
        .collect();
    let unquoted = strip_quotes(&separated);
    // Only ASCII bytes were replaced, by ASCII bytes.
    let unquoted =
        String::from_utf8(unquoted).map_err(|_| Unsupported("a term corpus which is not UTF-8"))?;
    let collapsed = collapse_unicode_space(&unquoted)?;
    let trimmed = collapsed.trim_matches(' ');
    if trimmed.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(" {trimmed} "))
    }
}

/// `PhutilSearchStemmer::applyStemmer()`.
fn apply_stemmer(token: &[u8]) -> Vec<u8> {
    // Tokens with internal punctuation (domain names, Conduit methods) are
    // kept as they are.
    if token.iter().any(|byte| matches!(byte, b'.' | b'_')) {
        return token.to_vec();
    }
    let stem = porter::stem(token);
    // Short stems are likely acronyms ("DNS").
    if stem.len() < 3 {
        return token.to_vec();
    }
    stem
}

/// `PhutilSearchStemmer::stemCorpus()`: the stems of the distinct words of
/// three bytes or more, separated by spaces.
pub fn stem_corpus(corpus: &str) -> Result<String, Unsupported> {
    let lowered = lowercase(corpus)?;
    // `preg_split('/[^a-zA-Z0-9\x7F-\xFF._]+/', ...)`, on bytes.
    let is_word = |byte: &u8| byte.is_ascii_alphanumeric() || *byte >= 0x7f || b"._".contains(byte);
    let mut words: Vec<&[u8]> = Vec::new();
    for token in lowered.as_bytes().split(|byte| !is_word(byte)) {
        let start = token
            .iter()
            .position(|byte| !b"._".contains(byte))
            .unwrap_or(token.len());
        let end = token
            .iter()
            .rposition(|byte| !b"._".contains(byte))
            .map_or(start, |end| end + 1);
        let token = &token[start..end.max(start)];
        if token.len() < 3 || words.contains(&token) {
            continue;
        }
        words.push(token);
    }
    let stems: Vec<Vec<u8>> = words.into_iter().map(apply_stemmer).collect();
    // The stemmer only removes or adds ASCII letters, and words split at
    // ASCII bytes.
    String::from_utf8(stems.join(&b' '))
        .map_err(|_| Unsupported("a stemmed corpus which is not UTF-8"))
}

/// `PhabricatorSearchNgramEngine::getTermNgramsFromString()`: the distinct
/// three-character ngrams of each word, padded with a space on each side,
/// in the order `ksort()` leaves them.
pub fn term_ngrams(text: &str) -> Result<Vec<String>, Unsupported> {
    let lowered = lowercase(text)?;
    let mut tokens: Vec<String> = Vec::new();
    for token in split_unicode_space(lowered.trim_matches(' '))? {
        let token = format!(" {token} ");
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }

    let mut ngrams: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in &tokens {
        let starts: Vec<usize> = token.char_indices().map(|(index, _)| index).collect();
        for window in 0..starts.len().saturating_sub(2) {
            let end = starts.get(window + 3).copied().unwrap_or(token.len());
            let ngram = &token[starts[window]..end];
            if seen.insert(ngram) {
                ngrams.push(ngram.to_owned());
            }
        }
    }
    php_sort::ksort_strings(&mut ngrams);
    Ok(ngrams)
}

/// `PhabricatorTypeaheadDatasource::tokenizeString()`: the distinct
/// lowercase words, split at white space, brackets, parentheses and
/// hyphens.
pub fn tokenize_string(text: &str) -> Result<Vec<String>, Unsupported> {
    let lowered = lowercase(text)?;
    // `trim()`'s default characters.
    let trimmed = lowered.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b']);
    // `preg_split('/[\s\[\]\(\)-]+/u', ...)`, then `array_unique()`, without
    // the empty token.
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for character in trimmed.chars() {
        if is_unicode_space(character)? || matches!(character, '[' | ']' | '(' | ')' | '-') {
            if !current.is_empty() && !tokens.contains(&current) {
                tokens.push(current.clone());
            }
            current.clear();
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() && !tokens.contains(&current) {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("", "")]
    #[case("  ", "")]
    #[case("Hello, World!", " Hello World ")]
    #[case("'quoted' and whom'st've", " quoted and whom'st've ")]
    #[case("a ''b'' c'", " a b c ")]
    #[case("x\u{a0}\u{2028}y", " x y ")]
    #[case("snake_case.method", " snake case method ")]
    fn builds_term_corpora(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(terms_corpus(raw).unwrap(), expected);
    }

    #[test]
    fn declines_case_folding_beyond_ascii() {
        assert!(lowercase("ÉTÉ").is_err());
        assert_eq!(lowercase("été 🎉 東京").unwrap(), "été 🎉 東京");
        assert!(terms_corpus("a\u{180e}b").is_err());
    }

    #[test]
    fn stems_distinct_words() {
        assert_eq!(
            stem_corpus("Running runners ran; the DNS of api.method_name").unwrap(),
            "run runner ran the dns api.method_name"
        );
        assert_eq!(stem_corpus("..ab.. __xyz").unwrap(), "xyz");
    }

    #[test]
    fn tokenizes_like_typeahead() {
        assert_eq!(
            tokenize_string("Web-Platform [tag] (x) web").unwrap(),
            ["web", "platform", "tag", "x"]
        );
        assert_eq!(tokenize_string(" \t").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn extracts_sorted_ngrams() {
        assert_eq!(
            term_ngrams("ab ab").unwrap(),
            vec![" ab".to_owned(), "ab ".to_owned()]
        );
        assert_eq!(term_ngrams("").unwrap(), Vec::<String>::new());
        assert_eq!(term_ngrams("é").unwrap(), vec![" é ".to_owned()]);
    }
}
