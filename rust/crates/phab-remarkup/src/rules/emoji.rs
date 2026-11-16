//! `PhabricatorEmojiRemarkupRule`: `:smile:` becomes the emoji.

use crate::engine::Context;
use crate::generated::emoji::EMOJI;
use crate::markup::Markup;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static EMOJI_NAME: Pattern = Pattern::new(r"\B:(\S+):\B");

/// `PhabricatorEmojiRemarkupRule`.
pub struct Emoji;

impl InlineRule for Emoji {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, _cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(
            &EMOJI_NAME,
            text.as_bytes(),
            |groups| {
                if !rule::is_flat_text(groups.get(0)) {
                    return groups.get(0).to_vec();
                }
                let name: Vec<u8> = groups
                    .get(1)
                    .iter()
                    .map(|&byte| if byte == b'-' { b'_' } else { byte })
                    .collect();
                EMOJI
                    .binary_search_by(|(key, _)| (*key).cmp(&name[..]))
                    .map_or_else(|_| groups.get(0).to_vec(), |index| EMOJI[index].1.to_vec())
            },
        ))
    }
}
