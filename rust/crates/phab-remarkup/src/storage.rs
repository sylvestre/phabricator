//! `PhutilRemarkupBlockStorage`: rules replace what they render with tokens
//! (`\x01<n>Z`), so later rules can't match inside it, and the tokens are
//! expanded at the end.

use crate::markup::Markup;
use crate::preg::{self, Pattern};

/// The byte which starts a token.
pub const MAGIC_BYTE: u8 = 0x01;

static TOKEN: Pattern = Pattern::new(r"\x01\d+Z");

/// The stored values, in the order they were stored.
#[derive(Debug, Default, Clone)]
pub struct BlockStorage {
    entries: Vec<Markup>,
}

impl BlockStorage {
    /// `store()`: keeps `value` and returns its token.
    pub fn store(&mut self, value: Markup) -> Vec<u8> {
        self.entries.push(value);
        format!("\x01{}Z", self.entries.len()).into_bytes()
    }

    /// `overwrite()`.
    pub fn overwrite(&mut self, key: &[u8], value: Markup) {
        if let Some(entry) = token_index(key).and_then(|index| self.entries.get_mut(index)) {
            *entry = value;
        }
    }

    /// `restore()`: expands every token of `corpus`. In HTML mode, text is
    /// escaped first and the result is safe HTML.
    ///
    /// # Panics
    ///
    /// If a token refers to a value stored after the one containing it,
    /// which rules never do (PHP throws too).
    pub fn restore(&self, corpus: &Markup, text_mode: bool) -> Markup {
        let mut expanded: Vec<Vec<u8>> = Vec::with_capacity(self.entries.len());
        let prepare = |value: &Markup| {
            if text_mode {
                value.as_bytes().to_vec()
            } else {
                value.escape()
            }
        };
        for value in &self.entries {
            let content = expand(&prepare(value), &expanded);
            expanded.push(content);
        }
        let output = expand(&prepare(corpus), &expanded);
        if text_mode {
            Markup::Text(output)
        } else {
            Markup::Html(output).into_safe()
        }
    }
}

/// Where the value of a token is: tokens are numbered from 1 in the order
/// they are stored.
fn token_index(key: &[u8]) -> Option<usize> {
    let digits = key.strip_prefix(&[MAGIC_BYTE])?.strip_suffix(b"Z")?;
    if digits.starts_with(b"0") {
        return None;
    }
    let number: usize = std::str::from_utf8(digits).ok()?.parse().ok()?;
    number.checked_sub(1)
}

/// Replaces the tokens of `content` with the values expanded so far.
fn expand(content: &[u8], expanded: &[Vec<u8>]) -> Vec<u8> {
    if !content.contains(&MAGIC_BYTE) {
        return content.to_vec();
    }
    preg::replace_callback(&TOKEN, content, |groups| {
        let key = groups.get(0);
        token_index(key)
            .and_then(|index| expanded.get(index))
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "token {:?} is not stored before the value using it",
                    String::from_utf8_lossy(key)
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_expand_inside_tokens() {
        let mut storage = BlockStorage::default();
        let inner = storage.store(Markup::html("<a>x</a>"));
        let mut outer = b"<em>".to_vec();
        outer.extend_from_slice(&inner);
        outer.extend_from_slice(b"</em>");
        let outer = storage.store(Markup::html(outer));
        let mut corpus = b"1 < 2 ".to_vec();
        corpus.extend_from_slice(&outer);
        assert_eq!(
            storage.restore(&Markup::text(corpus), false).as_bytes(),
            b"1 &lt; 2 <em><a>x</a></em>"
        );
    }
}
