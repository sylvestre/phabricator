//! `PhabricatorHandleRemarkupRule`: `{PHID-DREV-...}` shows the object's
//! name, linked, if the viewer may see it.

use crate::engine::Context;
use crate::generated::phid_types::PHID_TYPES;
use crate::markup::{self, Content, Markup};
use crate::objects::ObjectHandle;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};

static HANDLE: Pattern = Pattern::new(r"\{(PHID-[a-zA-Z0-9-]*)\}");

/// `PhabricatorHandleRemarkupRule`.
pub struct Handle;

/// `phid_get_type()` is a known type.
fn is_known_type(phid: &[u8]) -> bool {
    let parts: Vec<&[u8]> = phid.split(|byte| *byte == b'-').collect();
    parts.len() >= 3
        && std::str::from_utf8(parts[1]).is_ok_and(|kind| PHID_TYPES.binary_search(&kind).is_ok())
}

/// `PhabricatorObjectHandle::renderTag()`: a tag with the link name.
pub fn handle_tag(handle: &ObjectHandle, href: &[u8], name: &[u8]) -> Markup {
    let class: &[u8] = if handle.closed {
        b"remarkup-handle remarkup-handle-closed"
    } else {
        b"remarkup-handle"
    };
    markup::tag(
        "a",
        &[
            ("href", Some(href)),
            ("class", Some(class)),
            ("data-phid", Some(handle.phid.as_bytes())),
        ],
        Content::One(&Markup::text(name)),
    )
}

impl InlineRule for Handle {
    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&HANDLE, text.as_bytes(), |groups| {
            let phid = groups.get(1);
            if !rule::is_flat_text(groups.get(0)) || !is_known_type(phid) {
                return groups.get(0).to_vec();
            }
            let token = cx.store_text(Markup::text(groups.get(0)));
            if cx.is_text_mode() {
                return token;
            }
            let phid = String::from_utf8_lossy(phid).into_owned();
            cx.metadata
                .handles
                .entry(phid)
                .or_default()
                .push(token.clone());
            token
        }))
    }

    /// `didMarkupText()`: visible objects render, the others stay as
    /// written.
    fn did_markup_text(&self, cx: &mut Context<'_>) {
        for (phid, tokens) in std::mem::take(&mut cx.metadata.handles) {
            let Some(handle) = cx.resolved.handles.get(&phid).cloned() else {
                let original = [b"{", phid.as_bytes(), b"}"].concat();
                for token in &tokens {
                    cx.overwrite_stored_text(token, Markup::text(original.clone()));
                }
                continue;
            };
            let link = if cx.is_html_mail_mode() {
                markup::tag(
                    "a",
                    &[
                        ("href", Some(&cx.config().production_uri(&handle.uri))),
                        (
                            "style",
                            Some(
                                b"\n                border-color: #f1f7ff;\n                color: #19558d;\n                \
                                  background-color: #f1f7ff;\n                border: 1px solid transparent;\n                \
                                  border-radius: 3px;\n                font-weight: bold;\n                padding: 0 4px;",
                            ),
                        ),
                    ],
                    Content::One(&Markup::text(handle.link_name())),
                )
            } else {
                handle_tag(&handle, &handle.uri, handle.link_name())
            };
            for token in &tokens {
                cx.overwrite_stored_text(token, link.clone());
            }
        }
    }
}
