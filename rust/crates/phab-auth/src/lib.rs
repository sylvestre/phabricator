//! Authentication shared with the PHP application: Conduit API tokens
//! ([`token`]) and session cookies ([`session`]).
//!
//! Only the read side is ported. Paths PHP handles which these modules do
//! not (signed requests, cluster tokens, missing HMAC keys PHP would create)
//! come back as [`Outcome::Unsupported`], so the caller can hand the request
//! to PHP instead of answering differently.

mod error;
mod outcome;
pub mod session;
pub mod token;
pub mod user;

pub use error::AuthError;
pub use outcome::{Outcome, Writes};
pub use user::User;
