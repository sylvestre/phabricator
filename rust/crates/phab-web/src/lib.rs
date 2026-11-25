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
mod changeset_ajax;
mod changeset_view;
mod dashboards;
mod datasource;
mod diff_create;
mod diff_html;
mod diff_page;
mod diffusion;
mod error;
mod favorites;
mod feed;
mod files;
mod flags;
mod harbormaster;
mod herald;
mod highlight;
pub mod login;
mod notifications;
pub mod opt_in;
mod people;
mod people_lists;
pub mod php;
mod php_headers;
mod preview;
mod project_edit;
mod project_manage;
mod project_members;
mod projects;
mod queue;
mod raw_diff;
pub mod remarkup;
mod response;
mod revision;
mod revision_edit;
mod router;
mod search_form;
pub mod searchfox;
mod settings;
mod shell;
mod state;
mod subscriptions;
mod system;
mod time;
mod timeline;
mod transactions;
mod typeahead;
mod viewer;
mod web_edit;

pub use error::{FALLBACK_HEADER, PageError, fallback_response};
pub use response::html_response;
pub use router::router;
pub use shell::{Motd, Shell};
pub use state::WebState;
pub use viewer::WebUser;
