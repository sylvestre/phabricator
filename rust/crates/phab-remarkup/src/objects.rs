//! Object references (`D123`, `{D123}`, `#project`) and mentions (`@alice`)
//! wait in the storage while the text is marked up. The caller then loads
//! what they point to, as the viewer may see it, and the engine renders
//! them ([`crate::Engine::finish()`]): an object the viewer can't see stays
//! the text as written, as in PHP.

use std::collections::HashMap;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}
