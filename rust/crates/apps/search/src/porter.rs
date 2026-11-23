//! `externals/porter-stemmer/src/Porter.php`: Richard Heyes' PHP port of
//! the Porter stemmer, which `PhutilSearchStemmer` calls.
//!
//! The port keeps the PHP code's regular expressions and runs them with
//! PCRE2 on bytes, as PHP does, so a token with digits or bytes beyond
//! ASCII stems as it does there. `substr()` follows PHP 8 when the string
//! is shorter than the offsets.

use phab_remarkup::preg::{self, Pattern};

/// `$regex_consonant`.
macro_rules! consonant {
    () => {
        "(?:[bcdfghjklmnpqrstvwxz]|(?<=[aeiou])y|^y)"
    };
}

/// `$regex_vowel`.
macro_rules! vowel {
    () => {
        "(?:[aeiou]|(?<![aeiou])y)"
    };
}

static HAS_VOWEL: Pattern = Pattern::new(concat!(vowel!(), "+"));
static LEADING_CONSONANTS: Pattern = Pattern::new(concat!("^", consonant!(), "+"));
static TRAILING_VOWELS: Pattern = Pattern::new(concat!(vowel!(), "+$"));
static VOWELS_THEN_CONSONANTS: Pattern =
    Pattern::new(concat!("(", vowel!(), "+", consonant!(), "+)"));
static DOUBLE_CONSONANT: Pattern = Pattern::new(concat!(consonant!(), "{2}$"));
static CVC: Pattern = Pattern::new(concat!("(", consonant!(), vowel!(), consonant!(), ")$"));

/// PHP 8's `substr($string, $offset, $length)`.
fn substr(string: &[u8], offset: isize, length: Option<isize>) -> &[u8] {
    let size = string.len().cast_signed();
    if offset > size {
        return b"";
    }
    let start = if offset < 0 {
        (size + offset).max(0)
    } else {
        offset
    };
    let available = size - start;
    let length = match length {
        None => available,
        Some(length) if length < 0 => (available + length).max(0),
        Some(length) => length.min(available),
    };
    let start = start.cast_unsigned();
    &string[start..start + length.cast_unsigned()]
}

/// `Porter::Stem()`.
pub fn stem(word: &[u8]) -> Vec<u8> {
    if word.len() <= 2 {
        return word.to_vec();
    }
    let mut word = word.to_vec();
    step1ab(&mut word);
    step1c(&mut word);
    step2(&mut word);
    step3(&mut word);
    step4(&mut word);
    step5(&mut word);
    word
}

fn has_vowel(text: &[u8]) -> bool {
    preg::is_match(&HAS_VOWEL, text)
}

fn step1ab(word: &mut Vec<u8>) {
    // Part a.
    if substr(word, -1, None) == b"s" {
        let _ = replace(word, b"sses", b"ss", None)
            || replace(word, b"ies", b"i", None)
            || replace(word, b"ss", b"ss", None)
            || replace(word, b"s", b"", None);
    }

    // Part b.
    if substr(word, -2, Some(1)) != b"e" || !replace(word, b"eed", b"ee", Some(0)) {
        let removed = (has_vowel(substr(word, 0, Some(-3))) && replace(word, b"ing", b"", None))
            || (has_vowel(substr(word, 0, Some(-2))) && replace(word, b"ed", b"", None));
        if removed
            && !replace(word, b"at", b"ate", None)
            && !replace(word, b"bl", b"ble", None)
            && !replace(word, b"iz", b"ize", None)
        {
            let ending = substr(word, -2, None);
            if double_consonant(word) && ending != b"ll" && ending != b"ss" && ending != b"zz" {
                word.pop();
            } else if measure(word) == 1 && cvc(word) {
                word.push(b'e');
            }
        }
    }
}

fn step1c(word: &mut Vec<u8>) {
    if substr(word, -1, None) == b"y" && has_vowel(substr(word, 0, Some(-1))) {
        replace(word, b"y", b"i", None);
    }
}

/// Tries each `(suffix, replacement)` in turn until one ends the word, as
/// the `OR` chains do.
fn replace_first(word: &mut Vec<u8>, rules: &[(&[u8], &[u8])], m: usize) {
    for (check, replacement) in rules {
        if replace(word, check, replacement, Some(m)) {
            return;
        }
    }
}

fn step2(word: &mut Vec<u8>) {
    let rules: &[(&[u8], &[u8])] = match substr(word, -2, Some(1)) {
        b"a" => &[(b"ational", b"ate"), (b"tional", b"tion")],
        b"c" => &[(b"enci", b"ence"), (b"anci", b"ance")],
        b"e" => &[(b"izer", b"ize")],
        b"g" => &[(b"logi", b"log")],
        b"l" => &[
            (b"entli", b"ent"),
            (b"ousli", b"ous"),
            (b"alli", b"al"),
            (b"bli", b"ble"),
            (b"eli", b"e"),
        ],
        b"o" => &[(b"ization", b"ize"), (b"ation", b"ate"), (b"ator", b"ate")],
        b"s" => &[
            (b"iveness", b"ive"),
            (b"fulness", b"ful"),
            (b"ousness", b"ous"),
            (b"alism", b"al"),
        ],
        b"t" => &[(b"biliti", b"ble"), (b"aliti", b"al"), (b"iviti", b"ive")],
        _ => &[],
    };
    replace_first(word, rules, 0);
}

fn step3(word: &mut Vec<u8>) {
    let rules: &[(&[u8], &[u8])] = match substr(word, -2, Some(1)) {
        b"a" => &[(b"ical", b"ic")],
        b"s" => &[(b"ness", b"")],
        b"t" => &[(b"icate", b"ic"), (b"iciti", b"ic")],
        b"u" => &[(b"ful", b"")],
        b"v" => &[(b"ative", b"")],
        b"z" => &[(b"alize", b"al")],
        _ => &[],
    };
    replace_first(word, rules, 0);
}

fn step4(word: &mut Vec<u8>) {
    let rules: &[(&[u8], &[u8])] = match substr(word, -2, Some(1)) {
        b"a" => &[(b"al", b"")],
        b"c" => &[(b"ance", b""), (b"ence", b"")],
        b"e" => &[(b"er", b"")],
        b"i" => &[(b"ic", b"")],
        b"l" => &[(b"able", b""), (b"ible", b"")],
        b"n" => &[
            (b"ant", b""),
            (b"ement", b""),
            (b"ment", b""),
            (b"ent", b""),
        ],
        b"o" => {
            let ending = substr(word, -4, None);
            if ending == b"tion" || ending == b"sion" {
                &[(b"ion", b"")]
            } else {
                &[(b"ou", b"")]
            }
        }
        b"s" => &[(b"ism", b"")],
        b"t" => &[(b"ate", b""), (b"iti", b"")],
        b"u" => &[(b"ous", b"")],
        b"v" => &[(b"ive", b"")],
        b"z" => &[(b"ize", b"")],
        _ => &[],
    };
    replace_first(word, rules, 1);
}

fn step5(word: &mut Vec<u8>) {
    // Part a.
    if substr(word, -1, None) == b"e" {
        let stem = substr(word, 0, Some(-1));
        let m = measure(stem);
        if m > 1 || (m == 1 && !cvc(stem)) {
            replace(word, b"e", b"", None);
        }
    }

    // Part b.
    if measure(word) > 1 && double_consonant(word) && substr(word, -1, None) == b"l" {
        word.pop();
    }
}

/// `replace()`: when `word` ends with `check`, replaces it if what comes
/// before measures more than `m`; whether it ended with `check`.
fn replace(word: &mut Vec<u8>, check: &[u8], replacement: &[u8], m: Option<usize>) -> bool {
    // `substr($str, -strlen($check)) == $check`: the whole string when it
    // is shorter, which then can't be equal.
    if !word.ends_with(check) {
        return false;
    }
    let stem_length = word.len() - check.len();
    if m.is_none_or(|m| measure(&word[..stem_length]) > m) {
        word.truncate(stem_length);
        word.extend_from_slice(replacement);
    }
    true
}

/// `m()`: the number of vowel sequences followed by consonants.
fn measure(text: &[u8]) -> usize {
    let text = preg::replace(&LEADING_CONSONANTS, text, b"");
    let text = preg::replace(&TRAILING_VOWELS, &text, b"");
    preg::match_all(&VOWELS_THEN_CONSONANTS, &text).len()
}

/// `doubleConsonant()`.
fn double_consonant(text: &[u8]) -> bool {
    preg::captures(&DOUBLE_CONSONANT, text).is_some_and(|groups| {
        let pair = groups.get(0);
        pair.len() == 2 && pair[0] == pair[1]
    })
}

/// `cvc()`.
fn cvc(text: &[u8]) -> bool {
    preg::captures(&CVC, text).is_some_and(|groups| {
        let found = groups.get(1);
        found.len() == 3 && !(b'w'..=b'y').contains(&found[2])
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(b"", 0, None, b"")]
    #[case(b"a", -2, Some(1), b"a")]
    #[case(b"ab", -2, Some(1), b"a")]
    #[case(b"ab", 0, Some(-3), b"")]
    #[case(b"abcd", 0, Some(-3), b"a")]
    #[case(b"abc", 4, None, b"")]
    #[case(b"abc", 3, None, b"")]
    fn substr_follows_php(
        #[case] string: &[u8],
        #[case] offset: isize,
        #[case] length: Option<isize>,
        #[case] expected: &[u8],
    ) {
        assert_eq!(substr(string, offset, length), expected);
    }

    #[rstest]
    #[case("caresses", "caress")]
    #[case("ponies", "poni")]
    #[case("cats", "cat")]
    #[case("feed", "feed")]
    #[case("agreed", "agre")]
    #[case("plastered", "plaster")]
    #[case("motoring", "motor")]
    #[case("sing", "sing")]
    #[case("conflated", "conflat")]
    #[case("hopping", "hop")]
    #[case("filing", "file")]
    #[case("happy", "happi")]
    #[case("relational", "relat")]
    #[case("generalizations", "gener")]
    #[case("controlling", "control")]
    #[case("yes", "ye")]
    fn stems_like_porter(#[case] word: &str, #[case] expected: &str) {
        assert_eq!(stem(word.as_bytes()), expected.as_bytes());
    }
}
