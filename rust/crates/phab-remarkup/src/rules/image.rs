//! `PhabricatorImageRemarkupRule`: `{img https://...}` shows an external
//! image through the image proxy (`/file/imageproxy/`).

use indexmap::IndexMap;

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::options::{self, OptionValue};
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};
use crate::{php, uri};

static IMAGE: Pattern = Pattern::new(r"(?m)\{(image|img) ((?:[^}\\]+|\\.)*)\}");
static HTTP_URI: Pattern = Pattern::new(r"(?i)^https?\:\/\/.*\z");

/// `PhabricatorImageRemarkupRule`.
pub struct Image;

/// What `markupImage()` keeps of the options.
fn arguments(input: &[u8]) -> IndexMap<Vec<u8>, Vec<u8>> {
    let mut arguments = IndexMap::new();
    if preg::is_match(&HTTP_URI, input) {
        arguments.insert(b"uri".to_vec(), input.to_vec());
        return arguments;
    }
    let parsed = options::parse(input);
    // The last of "src", "uri" and "url" given wins.
    let uri = [&b"src"[..], b"uri", b"url"]
        .iter()
        .rev()
        .find_map(|key| parsed.get(*key));
    if let Some(uri) = uri {
        arguments.insert(b"uri".to_vec(), uri.to_php_string());
    }
    for (key, value) in parsed {
        arguments
            .entry(key)
            .or_insert_with(|| OptionValue::to_php_string(&value));
    }
    arguments
}

impl InlineRule for Image {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&IMAGE, text.as_bytes(), |groups| {
            if !rule::is_flat_text(groups.get(0)) {
                return groups.get(0).to_vec();
            }
            let arguments = arguments(php::trim(groups.get(2)));
            let Some(src) = arguments.get(&b"uri"[..]).filter(|uri| !uri.is_empty()) else {
                return groups.get(0).to_vec();
            };
            // Something that looks like a URI with a protocol; the proxy
            // checks it more carefully.
            match uri::protocol(src) {
                Ok(protocol) if !protocol.is_empty() => {}
                _ => return groups.get(0).to_vec(),
            }
            let proxy = [b"/file/imageproxy/?uri=", &uri::urlencode(src)[..]].concat();
            let attribute = |key: &[u8]| arguments.get(key).map(Vec::as_slice);
            let image = markup::tag(
                "img",
                &[
                    ("src", Some(&proxy)),
                    ("width", attribute(b"width")),
                    ("height", attribute(b"height")),
                    ("alt", attribute(b"alt")),
                    ("class", Some(b"remarkup-external-image")),
                ],
                Content::None,
            );
            cx.store_text(image)
        }))
    }
}
