//! `PhabricatorMentionRemarkupRule`: `@username`.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::objects::MentionedUser;
use crate::preg::{self, Pattern};
use crate::rule::InlineRule;

static MENTION: Pattern = Pattern::new(r"(?<!\w|@)@([a-zA-Z0-9._-]*[a-zA-Z0-9_-])");

/// `PhabricatorMentionRemarkupRule`.
pub struct Mention;

impl InlineRule for Mention {
    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(
            &MENTION,
            text.as_bytes(),
            |groups| {
                if cx.is_text_mode() {
                    return cx.store_text(Markup::text(groups.get(0)));
                }
                let token = cx.store_text(Markup::default());
                cx.metadata
                    .mention_originals
                    .insert(token.clone(), groups.get(1).to_vec());
                cx.metadata
                    .mentions
                    .entry(groups.get(1).to_ascii_lowercase())
                    .or_default()
                    .push(token.clone());
                token
            },
        ))
    }

    /// `didMarkupText()`: known users link to their profile, unknown names
    /// are marked as such.
    fn did_markup_text(&self, cx: &mut Context<'_>) {
        let mentions = std::mem::take(&mut cx.metadata.mentions);
        for (username, tokens) in &mentions {
            let Some(user) = cx.resolved.users.get(username).cloned() else {
                for token in tokens {
                    let original = cx.metadata.mention_originals.get(token).unwrap_or(username);
                    let tag = markup::tag(
                        "span",
                        &[("class", Some(b"phabricator-remarkup-mention-unknown"))],
                        Content::One(&Markup::Text([b"@", &original[..]].concat())),
                    );
                    cx.overwrite_stored_text(token, tag);
                }
                continue;
            };
            if !cx.metadata.mentioned_phids.contains(&user.phid) {
                cx.metadata.mentioned_phids.push(user.phid.clone());
            }
            let tag = user_tag(cx, &user);
            for token in tokens {
                cx.overwrite_stored_text(token, tag.clone());
            }
        }
    }
}

/// The link to a mentioned user.
fn user_tag(cx: &Context<'_>, user: &MentionedUser) -> Markup {
    let config = cx.config();
    let path = [b"/p/", &user.user_name[..], b"/"].concat();
    let name = Markup::Text([b"@", &user.user_name[..]].concat());
    if cx.is_html_mail_mode() {
        let colors = if user.can_view_context {
            "\n              border-color: #f1f7ff;\n              color: #19558d;\n              background-color: #f1f7ff;"
        } else {
            "\n              border-color: #92969D;\n              color: #92969D;\n              background-color: #F7F7F7;"
        };
        let style = format!(
            "{colors}\n                border: 1px solid transparent;\n                border-radius: 3px;\n                \
             font-weight: bold;\n                padding: 0 4px;"
        );
        return markup::tag(
            "a",
            &[
                ("href", Some(&config.production_uri(&path))),
                ("style", Some(style.as_bytes())),
            ],
            Content::One(&name),
        );
    }
    let href = if config.uri_full {
        config.full_uri(&path)
    } else {
        path
    };
    let mut class = b"remarkup-mention".to_vec();
    if user.is_disabled {
        class.extend_from_slice(b" remarkup-mention-disabled");
    }
    if !user.can_view_context {
        class.extend_from_slice(b" remarkup-mention-exiled");
    }
    markup::tag(
        "a",
        &[
            ("href", Some(&href)),
            ("class", Some(&class)),
            ("data-phid", Some(user.phid.as_bytes())),
        ],
        Content::One(&name),
    )
}
