//! `PhabricatorConfigApplication`: the configuration options PHP defines
//! (a catalog generated from `PhabricatorApplicationConfigOptions`, see
//! `rust/scripts/generate-catalogs.php`), and the two places `bin/config`
//! writes them, `conf/local/local.json` and the `config_entry` table.

pub mod catalog;
pub mod entry;
pub mod json;
pub mod local;
pub mod value;

pub use catalog::{ConfigOption, option, options};

/// Why configuration could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] phab_db::DbError),
    /// `local.json` could not be read or written.
    #[error("{0}")]
    Local(String),
    /// What the user asked for is not valid; the message says why, in
    /// PHP's words.
    #[error("{0}")]
    Usage(String),
    /// PHP would validate the value with code Rust doesn't have.
    #[error("not ported: {0}")]
    Unsupported(String),
}
