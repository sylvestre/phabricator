//! The web routes.

use std::sync::Arc;

use axum::{Router, routing};

use crate::state::WebState;
use crate::{assets, home, opt_in};

/// The pages, the static files and the interface switch. nginx decides
/// which of them reach the service (`rust/nginx/rust-web.conf`).
pub fn router<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/", routing::get(home::show::<S>))
        .route("/rust-ui/on", routing::get(opt_in::on))
        .route("/rust-ui/off", routing::get(opt_in::off))
        .route(
            &format!("{}/{{hash}}/{{name}}", assets::PREFIX),
            routing::get(assets::serve),
        )
}
