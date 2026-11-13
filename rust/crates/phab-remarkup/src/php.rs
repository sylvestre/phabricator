//! PHP string functions over bytes, as Remarkup uses them.

/// The characters `trim()` strips by default.
pub const WHITESPACE: &[u8] = b" \t\n\r\0\x0B";

/// `ltrim($text, $characters)`.
pub fn ltrim<'a>(text: &'a [u8], characters: &[u8]) -> &'a [u8] {
    let start = text
        .iter()
        .position(|byte| !characters.contains(byte))
        .unwrap_or(text.len());
    &text[start..]
}

/// `rtrim($text, $characters)`.
pub fn rtrim<'a>(text: &'a [u8], characters: &[u8]) -> &'a [u8] {
    let end = text
        .iter()
        .rposition(|byte| !characters.contains(byte))
        .map_or(0, |index| index + 1);
    &text[..end]
}

/// `trim($text, $characters)`.
pub fn trim_chars<'a>(text: &'a [u8], characters: &[u8]) -> &'a [u8] {
    rtrim(ltrim(text, characters), characters)
}

/// `trim($text)`.
pub fn trim(text: &[u8]) -> &[u8] {
    trim_chars(text, WHITESPACE)
}

/// `!strlen(trim($text))`.
pub fn is_blank(text: &[u8]) -> bool {
    trim(text).is_empty()
}

/// `phutil_split_lines($text, true)`: lines with their `\n`. An empty text
/// is one empty line.
pub fn split_lines(text: &[u8]) -> Vec<&[u8]> {
    if text.is_empty() {
        return vec![b""];
    }
    let mut lines: Vec<&[u8]> = text.split_inclusive(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// `explode("\n", $text)`.
pub fn explode_lines(text: &[u8]) -> Vec<&[u8]> {
    text.split(|byte| *byte == b'\n').collect()
}

/// `implode($glue, $pieces)`.
pub fn implode<T: AsRef<[u8]>>(glue: &[u8], pieces: &[T]) -> Vec<u8> {
    let mut output = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        if index > 0 {
            output.extend_from_slice(glue);
        }
        output.extend_from_slice(piece.as_ref());
    }
    output
}

/// `mb_strlen($text, 'UTF-8')`: invalid bytes count as one character each.
pub fn utf8_strlen(text: &[u8]) -> usize {
    text.utf8_chunks()
        .map(|chunk| chunk.valid().chars().count() + chunk.invalid().len())
        .sum()
}

/// `mb_convert_case($text, MB_CASE_LOWER, 'UTF-8')`.
pub fn utf8_strtolower(text: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(text.len());
    for chunk in text.utf8_chunks() {
        output.extend_from_slice(chunk.valid().to_lowercase().as_bytes());
        output.extend_from_slice(chunk.invalid());
    }
    output
}

/// `htmlspecialchars($text, ENT_QUOTES, 'UTF-8')`. Like PHP, text which is
/// not valid UTF-8 escapes to an empty string.
pub fn htmlspecialchars(text: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(text).is_err() {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(text.len());
    for &byte in text {
        match byte {
            b'&' => output.extend_from_slice(b"&amp;"),
            b'<' => output.extend_from_slice(b"&lt;"),
            b'>' => output.extend_from_slice(b"&gt;"),
            b'"' => output.extend_from_slice(b"&quot;"),
            b'\'' => output.extend_from_slice(b"&#039;"),
            _ => output.push(byte),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(b"".as_slice(), vec![b"".as_slice()])]
    #[case(b"a", vec![b"a".as_slice()])]
    #[case(b"a\n", vec![b"a\n".as_slice()])]
    #[case(b"a\n\nb", vec![b"a\n".as_slice(), b"\n", b"b"])]
    fn lines_keep_their_endings(#[case] text: &[u8], #[case] expected: Vec<&[u8]>) {
        assert_eq!(split_lines(text), expected);
    }

    #[test]
    fn escaping_matches_php() {
        assert_eq!(
            htmlspecialchars(b"<a href='x'>&\"</a>"),
            b"&lt;a href=&#039;x&#039;&gt;&amp;&quot;&lt;/a&gt;"
        );
        assert_eq!(htmlspecialchars(b"caf\xc3\xa9"), "café".as_bytes());
        assert_eq!(htmlspecialchars(b"\xff"), b"");
    }

    #[test]
    fn utf8_lengths_count_characters() {
        assert_eq!(utf8_strlen("héllo".as_bytes()), 5);
        assert_eq!(utf8_strlen(b"a\xffb"), 3);
        assert_eq!(utf8_strtolower("ÉcOLE".as_bytes()), "école".as_bytes());
    }

    #[test]
    fn trims_like_php() {
        assert_eq!(trim(b" \t a b \n\0"), b"a b");
        assert_eq!(trim_chars(b"== Head ==", b" =#"), b"Head");
        assert_eq!(rtrim(b"a|b||", b"|"), b"a|b");
    }
}
