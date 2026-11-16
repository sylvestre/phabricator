//! `PhabricatorYoutubeRemarkupRule`: a paragraph which is only a `YouTube`
//! link embeds the video (when `remarkup.enable-embedded-youtube` is on).

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule::InlineRule;
use crate::uri;

static YOUTUBE: Pattern = Pattern::new(r"(^|\.)youtube\.com\z");

/// `PhabricatorYoutubeRemarkupRule`.
pub struct Youtube;

impl InlineRule for Youtube {
    fn priority(&self) -> f64 {
        350.0
    }

    /// Like PHP, the whole text is read as one URI.
    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        let bytes = text.as_bytes();
        if uri::protocol(bytes).is_err() {
            return text;
        }
        let Some(domain) = uri::domain(bytes) else {
            return text;
        };
        if !preg::is_match(&YOUTUBE, &domain) {
            return text;
        }
        let videos: Vec<Vec<u8>> = uri::query_pairs(bytes)
            .into_iter()
            .filter(|(key, _)| key == b"v")
            .map(|(_, value)| value)
            .collect();
        let [video] = videos.as_slice() else {
            return text;
        };
        if cx.is_text_mode() || cx.is_html_mail_mode() {
            return text;
        }
        let src = [b"https://www.youtube.com/embed/", &video[..]].concat();
        let iframe = markup::tag(
            "iframe",
            &[
                ("width", Some(b"650")),
                ("height", Some(b"400")),
                ("style", Some(b"margin: 1em auto; border: 0px;")),
                ("src", Some(&src)),
                ("frameborder", Some(b"0")),
            ],
            Content::One(&Markup::default()),
        );
        let embed = markup::tag(
            "div",
            &[("class", Some(b"embedded-youtube-video"))],
            Content::One(&iframe),
        );
        Markup::Text(cx.store_text(embed))
    }
}
