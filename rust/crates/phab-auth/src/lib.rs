//! Authentication shared with the PHP application: Conduit API tokens
//! ([`token`]) and session cookies ([`session`]).
//!
//! Only the read side is ported. Paths PHP handles which these modules do
//! not (signed requests, cluster tokens, missing HMAC keys PHP would create)
//! come back as [`Outcome::Unsupported`], so the caller can hand the request
//! to PHP instead of answering differently.

pub mod session;
pub mod token;
pub mod user;

use phab_db::DbError;
use thiserror::Error;

pub use user::User;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Sqlx(#[from] phab_db::sqlx::Error),
    #[error(transparent)]
    Hash(#[from] phab_core::hash::HashError),
    #[error(transparent)]
    Config(#[from] phab_core::config::ConfigError),
}

/// Whether authentication may write to the database. PHP extends sessions,
/// upgrades legacy session keys and makes `cli-` tokens permanent while
/// authenticating; shadow traffic and read-only installs must not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    Allowed,
    Skipped,
}

/// Result of an authentication attempt which PHP may have handled
/// differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T> {
    Done(T),
    /// PHP must handle this request; the string says why.
    Unsupported(&'static str),
}
