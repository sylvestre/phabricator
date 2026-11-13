//! The web interface of the Rust service (`RUST_PORT_PLAN.md` §9).
//!
//! Rust renders its own pages instead of reproducing PHP's HTML. nginx sends
//! the ported pages here for users who opted in (the `phab_ui=rust` cookie,
//! see [`opt_in`]). A page which needs something the Rust code doesn't do
//! (a logged-out viewer, a pending multi-factor prompt, a missing HMAC key)
//! answers `421` through [`PageError::Fallback`] and nginx replays the
//! request against PHP, so the user sees the PHP page instead.

mod actions;
pub mod assets;
mod diff_html;
mod error;
mod feed;
pub mod opt_in;
mod people;
pub mod php;
mod projects;
mod queue;
pub mod remarkup;
mod response;
mod revision;
mod router;
mod shell;
mod state;
mod time;
mod timeline;
mod viewer;

pub use error::{FALLBACK_HEADER, PageError, fallback_response};
pub use response::html_response;
pub use router::router;
pub use shell::{Motd, Shell};
pub use state::WebState;
pub use viewer::WebUser;
