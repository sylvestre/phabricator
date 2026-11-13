//! The HTTP side of the Rust service.
//!
//! nginx sends the routes listed in `rust/nginx/*.conf` here. When a
//! request needs something the Rust code does not implement, the service
//! answers `421 Misdirected Request` with an `X-Phab-Fallback` header and
//! nginx replays the request against PHP.

mod routes;
pub mod shadow;
mod startup;

pub use phab_web::FALLBACK_HEADER;
pub use routes::{AppState, envelope_response, fallback_response, http_request, now, router};
pub use startup::{StartupError, build_context, default_config_dir, hostname};
