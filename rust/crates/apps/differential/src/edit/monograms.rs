//! `PhabricatorCustomFieldMonogramParser::parseCorpus()`, without the
//! regular expression: a prefix ("depends on", "reverts"), an optional
//! infix ("revision"), then monograms separated by commas and spaces, the
//! last possibly after "and".
//!
//! The pattern is `(?:^|\b)(?i:(PREFIX)\s+)(?i:(INFIX)\s+)?`
//! `((?:MONOGRAM(?:\b|$)[,\s]*)+)(?:\band\s+(MONOGRAM(?:\b|$)))?`, with no
//! suffix, matched in turn from where the last match ended.

/// A parser: its prefixes and infixes (lowercase, in PHP's order), and
/// where a monogram starting at an offset ends, if one does.
pub(super) struct Parser {
    pub(super) prefixes: &'static [&'static str],
    pub(super) infixes: &'static [&'static str],
    pub(super) monogram: fn(&[u8], usize) -> Option<usize>,
}

/// `\w` without the `u` flag.
pub(super) fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `\s` without the `u` flag.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0B' | b'\x0C' | b'\r')
}

/// Where a run of `\s` from `at` ends.
fn skip_spaces(text: &[u8], mut at: usize) -> usize {
    while text.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    at
}

impl Parser {
    /// The monograms of a match whose prefix ends at `at`, and where the
    /// match ends.
    fn monograms_after(
        &self,
        text: &[u8],
        lower: &[u8],
        at: usize,
    ) -> Option<(Vec<String>, usize)> {
        let after = skip_spaces(text, at);
        if after == at {
            return None;
        }
        let monogram = self.monogram;
        // An optional infix, followed by spaces, before the first monogram.
        let start = self
            .infixes
            .iter()
            .filter(|infix| lower[after..].starts_with(infix.as_bytes()))
            .map(|infix| after + infix.len())
            .map(|end| (end, skip_spaces(text, end)))
            .find(|&(end, next)| next > end && monogram(text, next).is_some())
            .map_or(after, |(_, next)| next);
        let mut found = Vec::new();
        let mut at = start;
        // `(?:MONOGRAM(?:\b|$)[,\s]*)+`
        while let Some(end) = monogram(text, at) {
            found.push(String::from_utf8_lossy(&text[at..end]).into_owned());
            at = end;
            while text
                .get(at)
                .is_some_and(|byte| *byte == b',' || is_space(*byte))
            {
                at += 1;
            }
        }
        if found.is_empty() {
            return None;
        }
        // `(?:\band\s+(MONOGRAM(?:\b|$)))?`
        if lower[at..].starts_with(b"and") && at > 0 && !is_word(text[at - 1]) {
            let next = skip_spaces(text, at + 3);
            if next > at + 3
                && let Some(end) = monogram(text, next)
            {
                found.push(String::from_utf8_lossy(&text[next..end]).into_owned());
                at = end;
            }
        }
        Some((found, at))
    }

    /// The monograms `corpus` names, in order, once each.
    pub(super) fn parse(&self, corpus: &str) -> Vec<String> {
        let text = corpus.as_bytes();
        let lower = corpus.to_ascii_lowercase().into_bytes();
        let mut monograms: Vec<String> = Vec::new();
        let mut at = 0;
        while at < text.len() {
            let bounded = at == 0 || !is_word(text[at - 1]);
            // The first prefix, in PHP's order, which leads to a match.
            let found = self
                .prefixes
                .iter()
                .filter(|_| bounded)
                .filter(|prefix| lower[at..].starts_with(prefix.as_bytes()))
                .find_map(|prefix| self.monograms_after(text, &lower, at + prefix.len()));
            if let Some((found, end)) = found {
                for monogram in found {
                    if !monograms.contains(&monogram) {
                        monograms.push(monogram);
                    }
                }
                at = end;
                continue;
            }
            at += 1;
        }
        monograms
    }
}

/// `[Dd]\d+(?:\b|$)` at `at`: where it ends.
fn revision(text: &[u8], at: usize) -> Option<usize> {
    if !matches!(text.get(at), Some(b'D' | b'd')) {
        return None;
    }
    let mut end = at + 1;
    while text.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let bounded = !text.get(end).copied().is_some_and(is_word);
    (end > at + 1 && bounded).then_some(end)
}

/// `[rA-Z0-9a-f]+(?:\b|$)` at `at`: every byte of the class is a word
/// byte, so backtracking finds no shorter match.
fn object(text: &[u8], at: usize) -> Option<usize> {
    let in_class = |byte: u8| {
        byte == b'r'
            || byte.is_ascii_uppercase()
            || byte.is_ascii_digit()
            || (b'a'..=b'f').contains(&byte)
    };
    let mut end = at;
    while text.get(end).copied().is_some_and(in_class) {
        end += 1;
    }
    let bounded = !text.get(end).copied().is_some_and(is_word);
    (end > at && bounded).then_some(end)
}

/// `DifferentialCustomFieldDependsOnParser`.
pub(super) const DEPENDS_ON: Parser = Parser {
    prefixes: &["depends on"],
    infixes: &[
        "diff", "diffs", "change", "changes", "rev", "revs", "revision",
    ],
    monogram: revision,
};

/// `DifferentialCustomFieldRevertsParser`: Git's "This reverts commit X",
/// Mercurial's "Backed out changeset Y".
pub(super) const REVERTS: Parser = Parser {
    prefixes: &[
        "revert",
        "reverts",
        "reverted",
        "backout",
        "backsout",
        "backedout",
        "back out",
        "backs out",
        "backed out",
        "undo",
        "undoes",
    ],
    infixes: &[
        "commit",
        "commits",
        "change",
        "changes",
        "changeset",
        "changesets",
        "rev",
        "revs",
        "revision",
        "revisions",
        "diff",
        "diffs",
    ],
    monogram: object,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reverts_like_php() {
        assert_eq!(REVERTS.parse("Reverts D2."), ["D2"]);
        assert_eq!(REVERTS.parse("Backs out D2 and D7."), ["D2", "D7"]);
        assert_eq!(REVERTS.parse("This undoes revision D6."), ["D6"]);
        assert_eq!(
            REVERTS.parse("This reverts commit 9f1c2e3d4b5a."),
            ["9f1c2e3d4b5a"]
        );
        assert_eq!(REVERTS.parse("Back out a typo."), ["a"]);
        assert_eq!(REVERTS.parse("Backed out changeset rVCT1234"), ["rVCT1234"]);
        for text in [
            "Keeps backward compatibility; the undo stack is unchanged.",
            "Unreverts D2",
            "Reverts the fix",
        ] {
            assert_eq!(REVERTS.parse(text), Vec::<String>::new(), "{text}");
        }
    }
}
