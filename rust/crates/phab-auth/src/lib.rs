//! Authentication shared with the PHP application: Conduit API tokens
//! ([`token`]), session cookies ([`session`]), CSRF tokens ([`csrf`]) and
//! the storage of the login workflow: providers ([`provider`]), the BMO
//! OAuth adapter ([`oauth`]), external accounts ([`external_account`]),
//! passwords ([`password`]), rate limits ([`rate_limit`]) and the activity
//! log ([`user_log`]).
//!
//! Paths PHP handles which these modules do not (signed requests, cluster
//! tokens, missing HMAC keys PHP would create, legacy password hashes)
//! come back as [`Outcome::Unsupported`], so the caller can hand the
//! request to PHP instead of answering differently.

pub mod csrf;
mod error;
pub mod external_account;
pub mod oauth;
mod outcome;
pub mod password;
pub mod provider;
pub mod rate_limit;
pub mod session;
pub mod token;
pub mod user;
pub mod user_log;

pub use error::AuthError;
pub use outcome::{Outcome, Writes};
pub use user::User;
