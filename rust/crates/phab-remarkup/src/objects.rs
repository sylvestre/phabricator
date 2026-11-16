//! Object references (`D123`, `{D123}`, `#project`) and mentions (`@alice`)
//! wait in the storage while the text is marked up. The caller then loads
//! what they point to, as the viewer may see it, and the engine renders
//! them ([`crate::Engine::finish()`]): an object the viewer can't see stays
//! the text as written, as in PHP.

use std::collections::HashMap;

use crate::preg::{self, Pattern};

/// The object names `PhabricatorObjectQuery::withNames()` knows, with the
/// prefix of the rule whose references they are, most specific first.
static NAMES: [(&str, Pattern); 8] = [
    ("D", Pattern::new(r"^D(\d*[1-9]\d*)\z")),
    ("H", Pattern::new(r"^H(\d*[1-9]\d*)\z")),
    ("S", Pattern::new(r"^S(\d*[1-9]\d*)\z")),
    ("F", Pattern::new(r"^F(\d*[1-9]\d*)\z")),
    ("r", Pattern::new(r"^r([A-Z]+)\z")),
    ("R", Pattern::new(r"^R(\d+)\z")),
    ("@", Pattern::new(r"^@(.+)\z")),
    (
        "",
        Pattern::new(
            r"^((?:r[A-Z]+:?|R[0-9]+:)[1-9]\d*|(?:r[A-Z]+:?|R[0-9]+:)[a-f0-9]{5,40}|[a-f0-9]{7,40})\z",
        ),
    ),
];

/// The rule prefix and the ID of an object name like `D123`, or `@` and
/// the username of `@alice`, for links to objects (`/D123`).
pub fn parse_object_name(name: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    NAMES.iter().find_map(|(prefix, pattern)| {
        preg::captures(pattern, name).map(|groups| (*prefix, groups.get(1).to_vec()))
    })
}

/// How an object is referred to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    /// `D123`, possibly with `#anchor`.
    Reference,
    /// `{D123}`, possibly with options.
    Embed,
}

/// An object reference found in the text
/// (`PhabricatorObjectRemarkupRule::markupObject()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMention {
    /// The rule's prefix, like `D` or `#`.
    pub prefix: &'static str,
    /// The ID as written.
    pub id: Vec<u8>,
    /// A reference or an embed.
    pub kind: ReferenceKind,
    /// The `#anchor` of a reference.
    pub anchor: Option<Vec<u8>>,
    /// The options of an embed.
    pub options: Option<Vec<u8>>,
    /// The text as written, shown if the object is not visible.
    pub original: Vec<u8>,
    /// The token in the storage.
    pub token: Vec<u8>,
    /// How deep in quotes the reference is.
    pub quote_depth: usize,
}

/// What the viewer may see of an object (`PhabricatorObjectHandle`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectHandle {
    /// The object's PHID.
    pub phid: String,
    /// `getName()`, like a project's name.
    pub name: Vec<u8>,
    /// `getFullName()`, like `D123: Fix the bug`.
    pub full_name: Vec<u8>,
    /// `getURI()`, like `/D123`.
    pub uri: Vec<u8>,
    /// `getStatus() == STATUS_CLOSED`.
    pub closed: bool,
    /// What a reference shows instead of the text as written
    /// (`getObjectNameText()`), like the name of a rewritten commit.
    pub reference_text: Option<Vec<u8>>,
}

impl ObjectHandle {
    /// `getLinkName()`: a user's name, the full name of anything else.
    pub fn link_name(&self) -> &[u8] {
        if self.phid.starts_with("PHID-USER-") {
            &self.name
        } else {
            &self.full_name
        }
    }
}

/// A file, for embeds (`{F123}`): what `PhabricatorEmbedFileRemarkupRule`
/// reads of a `PhabricatorFile`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileObject {
    /// `getName()`.
    pub name: Vec<u8>,
    /// `getMimeType()`.
    pub mime_type: Vec<u8>,
    /// `getBestURI()`: where the file is shown.
    pub best_uri: Vec<u8>,
    /// `getDownloadURI()`.
    pub download_uri: Vec<u8>,
    /// The preview thumbnail (`TRANSFORM_PREVIEW`), when the file is an
    /// image.
    pub preview: Option<Preview>,
    /// `getImageWidth()` and `getImageHeight()`.
    pub image_size: Option<(u32, u32)>,
    /// `isViewableImage()`.
    pub is_viewable_image: bool,
    /// `isAudio()`.
    pub is_audio: bool,
    /// `isVideo()`.
    pub is_video: bool,
    /// `getByteSize()`.
    pub byte_size: u64,
    /// `getAltText()`.
    pub alt_text: Vec<u8>,
}

/// A thumbnail of an image file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// Where it is.
    pub uri: Vec<u8>,
    /// Its width and height, if known, as PHP prints them (they may be
    /// fractions).
    pub size: Option<(Vec<u8>, Vec<u8>)>,
}

/// A repository, for source links (`{src ...}` and links to Diffusion).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryRef {
    /// Its PHID.
    pub phid: String,
    /// `getMonogram()`, like `rMOZILLACENTRAL` or `R12`.
    pub monogram: Vec<u8>,
    /// `getURI()`, like `/source/mozilla-central/`.
    pub uri: Vec<u8>,
    /// `getDefaultBranch()` of repositories with branches (Git and
    /// Mercurial, `supportsRefs()`).
    pub default_branch: Option<Vec<u8>>,
    /// The Searchfox tree showing its code, like
    /// `https://searchfox.org/mozilla-central/`: `{src}` links go there
    /// instead of Diffusion.
    pub searchfox: Option<Vec<u8>>,
}

/// A mentioned user, for `PhabricatorMentionRemarkupRule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionedUser {
    /// The user's PHID.
    pub phid: String,
    /// The username, with its case.
    pub user_name: Vec<u8>,
    /// Whether the account is disabled.
    pub is_disabled: bool,
    /// Whether the user can see the object the text belongs to; a user who
    /// can't is shown as such.
    pub can_view_context: bool,
}

/// The objects and users the caller found for the references of a text.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// The visible objects, by rule prefix and ID as written.
    pub objects: HashMap<(&'static str, Vec<u8>), ObjectHandle>,
    /// The users, by lowercased username.
    pub users: HashMap<Vec<u8>, MentionedUser>,
    /// The objects of `{PHID-...}`, by PHID.
    pub handles: HashMap<String, ObjectHandle>,
    /// The files of `{F123}` embeds, by ID as written.
    pub files: HashMap<Vec<u8>, FileObject>,
    /// The repositories of source links, by identifier as written.
    pub repositories: HashMap<Vec<u8>, RepositoryRef>,
}
