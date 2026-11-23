//! Authentication shared with the PHP application: Conduit API tokens
//! ([`token`]), session cookies ([`session`]), CSRF tokens ([`csrf`]) and
//! the storage of the login workflow: providers ([`provider`]), the BMO
//! OAuth adapter ([`oauth`]), external accounts ([`external_account`]),
//! passwords ([`password`]), rate limits ([`rate_limit`]), the activity
//! log ([`user_log`]), multi-factor authentication with TOTP ([`mfa`],
//! [`totp`]), account registration ([`register`]) and the actions of the
//! account settings panels: Conduit API tokens ([`conduit_token`]),
//! email addresses ([`email`]) and sessions ([`session_actions`]).
//!
//! Paths PHP handles which these modules do not (signed requests, cluster
//! tokens, missing HMAC keys PHP would create, legacy password hashes)
//! come back as [`Outcome::Unsupported`], so the caller can hand the
//! request to PHP instead of answering differently.

pub mod conduit_token;
pub mod csrf;
pub mod email;
mod error;
pub mod external_account;
pub mod mfa;
pub mod oauth;
mod outcome;
pub mod password;
pub mod provider;
pub mod rate_limit;
pub mod register;
pub mod session;
pub mod session_actions;
pub mod token;
pub mod totp;
pub mod user;
pub mod user_log;

pub use error::AuthError;
pub use outcome::{Outcome, Writes};
pub use user::User;
