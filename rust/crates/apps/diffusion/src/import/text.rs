//! The libphutil string functions the commit parsers use:
//! `PhutilUTF8StringTruncator`, `phutil_split_lines()`,
//! `PhabricatorRepositoryCommitData::summarizeCommitMessage()`.

/// `phutil_utf8_is_combining_character()`.
fn is_combining(character: char) -> bool {
    matches!(
        u32::from(character),
        0x0300..=0x036F | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

/// `phutil_utf8v($string, $byte_limit)`: the characters, up to the first
/// one which reaches `byte_limit` bytes.
fn utf8v(text: &str, byte_limit: Option<usize>) -> Vec<&str> {
    let mut characters = Vec::new();
    for (start, character) in text.char_indices() {
        let end = start + character.len_utf8();
        characters.push(&text[start..end]);
        if byte_limit.is_some_and(|limit| limit > 0 && end >= limit) {
            break;
        }
    }
    characters
}

/// `phutil_utf8v_combine_characters()`.
fn combine(characters: &[&str]) -> Vec<String> {
    let Some(first) = characters.first() else {
        return Vec::new();
    };
    let combining = |character: &str| character.chars().any(is_combining);
    let mut buffer: Option<String> = combining(first).then(|| " ".to_owned());
    let mut parts = Vec::new();
    for character in characters {
        if character.len() > 1 && combining(character) {
            buffer.get_or_insert_with(String::new).push_str(character);
        } else {
            if let Some(previous) = buffer.take() {
                parts.push(previous);
            }
            buffer = Some((*character).to_owned());
        }
    }
    parts.extend(buffer);
    parts
}

/// `PhutilUTF8StringTruncator` with the default terminator (`…`): at most
/// `maximum_bytes` bytes and `maximum_glyphs` glyphs (no limit for `None`).
pub fn truncate(text: &str, maximum_bytes: Option<usize>, maximum_glyphs: Option<usize>) -> String {
    const TERMINATOR: &str = "\u{2026}";
    const TERMINATOR_BYTES: usize = 3;
    const TERMINATOR_GLYPHS: usize = 1;
    let minimum_limit = [maximum_bytes, maximum_glyphs]
        .into_iter()
        .flatten()
        .filter(|limit| *limit > 0)
        .min();
    let byte_len = text.len();
    if minimum_limit.is_none_or(|limit| byte_len <= limit) {
        return text.to_owned();
    }
    let hard_limit = maximum_bytes
        .filter(|limit| *limit > 0)
        .map(|limit| limit + 32);
    let points = utf8v(text, hard_limit);
    let glyphs = combine(&points);
    let glyph_len = glyphs.len();
    let too_long = maximum_bytes.is_some_and(|limit| limit > 0 && byte_len > limit)
        || maximum_glyphs.is_some_and(|limit| limit > 0 && glyph_len > limit);
    if !too_long {
        return text.to_owned();
    }
    let mut cutoff = glyph_len;
    if let Some(limit) = maximum_bytes.filter(|limit| *limit > 0)
        && byte_len > limit
    {
        let mut bytes = TERMINATOR_BYTES;
        for (index, glyph) in glyphs.iter().enumerate() {
            bytes += glyph.len();
            if bytes > limit {
                cutoff = index;
                break;
            }
        }
    }
    if let Some(limit) = maximum_glyphs.filter(|limit| *limit > 0)
        && glyph_len > limit
    {
        cutoff = cutoff.min(limit.saturating_sub(TERMINATOR_GLYPHS));
    }
    if cutoff == 0 {
        return TERMINATOR.to_owned();
    }
    let is_break = |glyph: &str| matches!(glyph, " " | "\n" | ";" | ":" | "[" | "(" | "," | "-");
    let is_stop = |glyph: &str| matches!(glyph, "." | "!" | "?");
    let mut word_boundary = None;
    let mut stop_boundary = None;
    let mut any_nonboundary = false;
    // `max(0, $cutoff - 256, $cutoff / 2)`, a float when odd: the loop
    // stops above it.
    let search = cutoff.saturating_sub(256).max(cutoff.div_ceil(2));
    let mut index = cutoff.min(glyph_len.saturating_sub(1));
    loop {
        if index < search {
            break;
        }
        let glyph = glyphs[index].as_str();
        if is_break(glyph) {
            word_boundary = Some(index);
        } else if is_stop(glyph) {
            stop_boundary = Some(index + 1);
            break;
        } else {
            any_nonboundary = true;
            if word_boundary.is_some() {
                break;
            }
        }
        if index == 0 {
            break;
        }
        index -= 1;
    }
    if let Some(stop) = stop_boundary {
        return glyphs[..stop].concat();
    }
    let boundary = match word_boundary {
        Some(boundary) if any_nonboundary => boundary,
        _ => cutoff,
    };
    let mut result = glyphs[..boundary].concat();
    result.push_str(TERMINATOR);
    result
}

/// `phutil_split_lines($text, false)`.
pub fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// `PhabricatorRepositoryCommitData::summarizeCommitMessage()`: the first
/// line, cut to fit the `summary` column (255 bytes) and 80 glyphs.
pub fn summarize_commit_message(message: &str) -> String {
    let first = split_lines(message).first().copied().unwrap_or_default();
    truncate(first, Some(255), Some(80))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_like_php() {
        assert_eq!(truncate("short", Some(255), Some(80)), "short");
        let long = "word ".repeat(30);
        // 80 glyphs: the cutoff is 79, PHP breaks at the space before it.
        assert_eq!(
            truncate(&long, Some(255), Some(80)),
            format!("{}word\u{2026}", "word ".repeat(15))
        );
        assert_eq!(
            truncate(&"x".repeat(100), Some(255), Some(80)),
            format!("{}\u{2026}", "x".repeat(79))
        );
        assert_eq!(
            truncate(&"y".repeat(300), Some(255), None),
            format!("{}\u{2026}", "y".repeat(252))
        );
        assert_eq!(truncate("Fix it. Then more", None, Some(10)), "Fix it.");
        assert_eq!(
            truncate("Fix it. Then more words follow here", None, Some(20)),
            "Fix it. Then more\u{2026}"
        );
    }

    #[test]
    fn splits_lines_like_php() {
        assert_eq!(split_lines(""), [""]);
        assert_eq!(split_lines("a\r\nb\n"), ["a", "b"]);
        assert_eq!(split_lines("\n\nx"), ["", "", "x"]);
        assert_eq!(summarize_commit_message("Subject\n\nBody"), "Subject");
    }
}
