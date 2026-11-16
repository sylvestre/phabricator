//! Links to the install itself (`PhabricatorSelfHyperlinkEngineExtension`
//! and `DiffusionSourceHyperlinkEngineExtension`): `https://phab/D123`
//! shows like a reference to D123, and links to files in Diffusion like
//! source links.

use crate::engine::Context;
use crate::markup::Markup;
use crate::objects::{self, ObjectHandle};
use crate::preg::{self, Pattern};
use crate::rules::handle;
use crate::rules::hyperlink::LinkHandler;
use crate::rules::source_link::{self, BROWSE_PATH};
use crate::uri;

static OBJECT_PATH: Pattern = Pattern::new(r"^/([^/]+)\z");

/// `getSelfLinks()`: links to the install, over HTTP or HTTPS.
pub fn is_self_link(cx: &Context<'_>, link: &[u8]) -> bool {
    cx.config().is_self_uri(link)
        && matches!(uri::protocol(link).as_deref(), Ok(b"http" | b"https"))
}

/// The object name of a link like `https://phab/D123`.
pub fn object_name(link: &[u8]) -> Option<Vec<u8>> {
    let path = uri::path(link);
    preg::captures(&OBJECT_PATH, &path).map(|groups| groups.get(1).to_vec())
}

/// The repository and the blob of a link to a file in Diffusion.
pub fn browse_target(link: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let path = uri::path(link);
    preg::captures(&BROWSE_PATH, &path)
        .map(|groups| (groups.get(1).to_vec(), groups.get(2).to_vec()))
}

/// `PhabricatorSelfHyperlinkEngineExtension`.
pub struct SelfObjects;

impl LinkHandler for SelfObjects {
    fn render(&self, cx: &Context<'_>, link: &[u8], embed: bool) -> Option<Markup> {
        if !is_self_link(cx, link) {
            return None;
        }
        let name = object_name(link)?;
        let handle: ObjectHandle = match objects::parse_object_name(&name)? {
            ("@", username) => {
                let user = cx.resolved.users.get(&username.to_ascii_lowercase())?;
                ObjectHandle {
                    phid: user.phid.clone(),
                    name: user.user_name.clone(),
                    full_name: user.user_name.clone(),
                    uri: [b"/p/", &user.user_name[..], b"/"].concat(),
                    closed: user.is_disabled,
                    reference_text: None,
                }
            }
            (prefix, id) => cx.resolved.objects.get(&(prefix, id))?.clone(),
        };
        let text = if embed { handle.link_name() } else { link };
        Some(handle::handle_tag(&handle, link, text))
    }
}

/// `DiffusionSourceHyperlinkEngineExtension`.
pub struct SelfSources;

impl LinkHandler for SelfSources {
    fn render(&self, cx: &Context<'_>, link: &[u8], embed: bool) -> Option<Markup> {
        if !is_self_link(cx, link) {
            return None;
        }
        let (identifier, blob) = browse_target(link)?;
        let repository = cx.resolved.repositories.get(&identifier)?;
        let location = source_link::parse_blob(&blob, repository.default_branch.is_some());
        let text = (!embed).then_some(link);
        Some(source_link::render_view(repository, link, &location, text))
    }
}
