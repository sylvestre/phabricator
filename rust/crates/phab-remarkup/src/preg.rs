//! PHP's `preg_*` functions over byte strings.
//!
//! PHP runs PCRE2 on bytes (without the `u` flag, which Remarkup doesn't
//! use), so these wrap the same library with the same semantics: `.`
//! matches one byte, `\s`, `\w` and `\b` are ASCII. Patterns are the PHP
//! ones with their flags written inline (`(?s)`, `(?m)`, `(?U)`).
//!
//! A match failing on PCRE2's limits counts as no match. PHP returns `null`
//! there instead, which Remarkup callers mostly treat like an empty result.

use std::sync::OnceLock;

use pcre2::bytes::{CaptureLocations, Regex, RegexBuilder};

/// A pattern, compiled the first time it is used, so it can be a `static`.
pub struct Pattern {
    source: &'static str,
    regex: OnceLock<Regex>,
}

impl Pattern {
    /// A pattern from PCRE2 syntax with inline flags.
    pub const fn new(source: &'static str) -> Self {
        Self {
            source,
            regex: OnceLock::new(),
        }
    }

    /// The compiled pattern.
    ///
    /// # Panics
    ///
    /// If the pattern doesn't compile: patterns are constants of this
    /// crate, and the tests compile each of them.
    pub fn regex(&self) -> &Regex {
        self.regex.get_or_init(|| compile(self.source))
    }
}

/// Compiles a pattern built at run time.
///
/// # Panics
///
/// If the pattern doesn't compile.
pub fn compile(source: &str) -> Regex {
    RegexBuilder::new()
        .jit_if_available(true)
        .build(source)
        .unwrap_or_else(|error| panic!("invalid pattern {source:?}: {error}"))
}

/// The groups of one match, as PHP's `$matches` sees them.
#[derive(Debug, Clone)]
pub struct Groups<'s> {
    subject: &'s [u8],
    spans: Vec<Option<(usize, usize)>>,
}

impl<'s> Groups<'s> {
    fn read(subject: &'s [u8], locations: &CaptureLocations) -> Self {
        let mut spans: Vec<_> = (0..locations.len()).map(|i| locations.get(i)).collect();
        // PHP leaves trailing groups which did not take part out of
        // `$matches`.
        while spans.len() > 1 && spans.last().is_some_and(Option::is_none) {
            spans.pop();
        }
        Self { subject, spans }
    }

    /// `$matches[$i]`: the group's text, or an empty string if it did not
    /// take part.
    pub fn get(&self, index: usize) -> &'s [u8] {
        match self.spans.get(index) {
            Some(Some((start, end))) => &self.subject[*start..*end],
            _ => b"",
        }
    }

    /// `isset($matches[$i])`.
    pub fn is_set(&self, index: usize) -> bool {
        index < self.spans.len()
    }

    /// Where the whole match starts.
    pub fn start(&self) -> usize {
        self.spans[0].map_or(0, |(start, _)| start)
    }

    /// Where the whole match ends.
    pub fn end(&self) -> usize {
        self.spans[0].map_or(0, |(_, end)| end)
    }

    /// `count($matches)`.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether only the whole match is present.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

fn captures_at<'s>(regex: &Regex, subject: &'s [u8], start: usize) -> Option<Groups<'s>> {
    let mut locations = regex.capture_locations();
    match regex.captures_read_at(&mut locations, subject, start) {
        Ok(Some(_)) => Some(Groups::read(subject, &locations)),
        Ok(None) => None,
        Err(error) => {
            tracing_error(regex, &error);
            None
        }
    }
}

fn tracing_error(regex: &Regex, error: &pcre2::Error) {
    tracing::warn!(pattern = regex.as_str(), %error, "pattern failed");
}

/// `preg_match()`.
pub fn is_match(pattern: &Pattern, subject: &[u8]) -> bool {
    captures_at(pattern.regex(), subject, 0).is_some()
}

/// `preg_match()` with `$matches`.
pub fn captures<'s>(pattern: &Pattern, subject: &'s [u8]) -> Option<Groups<'s>> {
    captures_at(pattern.regex(), subject, 0)
}

/// `preg_match()` with an offset, where `\G` anchors.
pub fn captures_from<'s>(
    pattern: &Pattern,
    subject: &'s [u8],
    offset: usize,
) -> Option<Groups<'s>> {
    captures_at(pattern.regex(), subject, offset)
}

/// Every match from left to right, as `preg_match_all()` and
/// `preg_replace()` walk them: after an empty match, the next search starts
/// one byte later.
fn all<'s>(regex: &Regex, subject: &'s [u8]) -> Vec<Groups<'s>> {
    let mut found = Vec::new();
    let mut start = 0;
    while start <= subject.len() {
        let Some(groups) = captures_at(regex, subject, start) else {
            break;
        };
        start = if groups.end() == groups.start() {
            groups.end() + 1
        } else {
            groups.end()
        };
        found.push(groups);
    }
    found
}

/// `preg_match_all()`, in `PREG_SET_ORDER`.
pub fn match_all<'s>(pattern: &Pattern, subject: &'s [u8]) -> Vec<Groups<'s>> {
    all(pattern.regex(), subject)
}

/// `preg_replace_callback()`.
pub fn replace_callback(
    pattern: &Pattern,
    subject: &[u8],
    mut callback: impl FnMut(&Groups<'_>) -> Vec<u8>,
) -> Vec<u8> {
    replace_with_regex(pattern.regex(), subject, &mut callback)
}

/// `preg_replace_callback()` with a pattern compiled at run time.
pub fn replace_callback_regex(
    regex: &Regex,
    subject: &[u8],
    mut callback: impl FnMut(&Groups<'_>) -> Vec<u8>,
) -> Vec<u8> {
    replace_with_regex(regex, subject, &mut callback)
}

fn replace_with_regex(
    regex: &Regex,
    subject: &[u8],
    callback: &mut dyn FnMut(&Groups<'_>) -> Vec<u8>,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(subject.len());
    let mut copied = 0;
    for groups in all(regex, subject) {
        output.extend_from_slice(&subject[copied..groups.start()]);
        output.extend_from_slice(&callback(&groups));
        copied = groups.end();
    }
    output.extend_from_slice(&subject[copied..]);
    output
}

/// `preg_replace()`, where `\1` or `$1` in the replacement stand for a
/// group.
pub fn replace(pattern: &Pattern, subject: &[u8], replacement: &[u8]) -> Vec<u8> {
    replace_callback(pattern, subject, |groups| expand(replacement, groups))
}

/// `preg_replace()` with a pattern compiled at run time.
pub fn replace_regex(regex: &Regex, subject: &[u8], replacement: &[u8]) -> Vec<u8> {
    replace_callback_regex(regex, subject, |groups| expand(replacement, groups))
}

fn expand(replacement: &[u8], groups: &Groups<'_>) -> Vec<u8> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < replacement.len() {
        let byte = replacement[index];
        if (byte == b'\\' || byte == b'$')
            && replacement.get(index + 1).is_some_and(u8::is_ascii_digit)
        {
            let digits = replacement[index + 1..]
                .iter()
                .take(2)
                .take_while(|digit| digit.is_ascii_digit())
                .count();
            let number = replacement[index + 1..index + 1 + digits]
                .iter()
                .fold(0, |number, digit| number * 10 + usize::from(digit - b'0'));
            output.extend_from_slice(groups.get(number));
            index += 1 + digits;
        } else {
            output.push(byte);
            index += 1;
        }
    }
    output
}

/// `preg_split()` without flags, for patterns which can't match empty.
pub fn split<'s>(pattern: &Pattern, subject: &'s [u8]) -> Vec<&'s [u8]> {
    let mut pieces = Vec::new();
    let mut last = 0;
    for groups in all(pattern.regex(), subject) {
        pieces.push(&subject[last..groups.start()]);
        last = groups.end();
    }
    pieces.push(&subject[last..]);
    pieces
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn groups_follow_php() {
        let pattern = Pattern::new(r"(a)|(b)(c)?");
        let groups = captures(&pattern, b"b").unwrap();
        assert_eq!(groups.get(0), b"b");
        assert!(groups.is_set(1));
        assert_eq!(groups.get(1), b"");
        assert!(groups.is_set(2));
        assert!(!groups.is_set(3));
    }

    #[rstest]
    #[case(r"(?m)^", b"a\nb".as_slice(), b"  ".as_slice(), b"  a\n  b".as_slice())]
    #[case(r"x*", b"abc", b"-", b"-a-b-c-")]
    #[case(r" *(\n+) *", b"a \n\n b", b"\\1", b"a\n\nb")]
    #[case(r"(?<!\n)\n(?!\n)", b"a\nb\n\nc", b" ", b"a b\n\nc")]
    fn replacements_match_php(
        #[case] pattern: &'static str,
        #[case] subject: &[u8],
        #[case] replacement: &[u8],
        #[case] expected: &[u8],
    ) {
        assert_eq!(
            replace(&Pattern::new(pattern), subject, replacement),
            expected
        );
    }

    #[test]
    fn split_matches_php() {
        let pattern = Pattern::new(r"\n{2,}");
        assert_eq!(
            split(&pattern, b"a\n\nb\n\n\nc"),
            [b"a".as_slice(), b"b", b"c"]
        );
        assert_eq!(split(&pattern, b"a"), [b"a".as_slice()]);
    }
}
