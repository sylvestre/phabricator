//! `PhabricatorSettingsApplication`: user preferences, how the settings
//! panels show and save them, and the lists of the account panels.

pub mod account;
pub mod editor;
pub mod editor_uri;
pub mod mail_tags;
pub mod preferences;
pub mod setting;
mod zones;

use phab_core::PhidType;

/// `PhabricatorUserPreferencesPHIDType`.
pub const PREFERENCES: PhidType = PhidType::new(*b"PSET");

/// Why settings could not be read or saved.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] phab_db::DbError),
    /// The stored data is something PHP would handle in a way Rust doesn't.
    #[error("not ported: {0}")]
    Unsupported(&'static str),
}
