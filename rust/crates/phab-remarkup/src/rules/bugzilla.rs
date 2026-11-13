//! `PhabricatorBugzillaLinkRule` (moz-extensions): `bug 123456` and
//! `bug 123456 comment 7` link to Bugzilla.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule::InlineRule;
use crate::uri;

static BUG: Pattern = Pattern::new(r"(?i)\bbug\s*#?\s*(\d+)(\s*comment\s*\#?\s*(\d+))?\b");

/// `PhabricatorBugzillaLinkRule`.
pub struct BugzillaLink;

impl InlineRule for BugzillaLink {
    fn priority(&self) -> f64 {
        360.0
    }

    /// Like PHP, the link is stored in text mode too, so text gets the
    /// HTML of the link.
    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&BUG, text.as_bytes(), |groups| {
            let mut href =
                uri::origin(cx.config().bugzilla_url.as_deref().unwrap_or("").as_bytes());
            href.extend_from_slice(b"/show_bug.cgi?id=");
            href.extend_from_slice(groups.get(1));
            if groups.len() == 4 && !groups.get(3).is_empty() && groups.get(3) != b"0" {
                href.extend_from_slice(b"#c");
                href.extend_from_slice(groups.get(3));
            }
            let link = markup::tag(
                "a",
                &[("href", Some(&href))],
                Content::One(&Markup::text(groups.get(0))),
            );
            cx.store_text(link)
        }))
    }
}
