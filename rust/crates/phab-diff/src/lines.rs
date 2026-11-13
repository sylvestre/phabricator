//! String helpers with PHP semantics.

/// `phutil_split_lines($corpus, true)`: splits after each `\n`, keeping the
/// line endings. A trailing empty piece is dropped, but an empty corpus is
/// one empty line, as in PHP.
pub fn split_lines(corpus: &str) -> Vec<&str> {
    if corpus.is_empty() {
        return vec![""];
    }
    corpus.split_inclusive('\n').collect()
}

/// PHP's `ltrim()` with its default character list.
pub(crate) fn php_ltrim(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("", &[""])]
    #[case("a", &["a"])]
    #[case("a\n", &["a\n"])]
    #[case("a\r\nb\n\nc", &["a\r\n", "b\n", "\n", "c"])]
    fn splits_like_php(#[case] corpus: &str, #[case] expected: &[&str]) {
        assert_eq!(split_lines(corpus), expected);
    }

    #[test]
    fn ltrim_matches_php() {
        assert_eq!(php_ltrim(" \t\r\n\0\x0Bx y "), "x y ");
    }
}
