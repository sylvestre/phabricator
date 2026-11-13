//! The web routes.

use std::sync::Arc;

use axum::{Router, routing};

use crate::state::WebState;
use crate::{assets, home, opt_in, people, projects};

/// The pages, the static files and the interface switch. nginx decides
/// which of them reach the service (`rust/nginx/rust-web.conf`).
pub fn router<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/", routing::get(home::show::<S>))
        .route("/p/{username}/", routing::get(people::show::<S>))
        .route("/tag/{slug}/", routing::get(projects::by_slug::<S>))
        .route("/project/view/{id}/", routing::get(projects::by_id::<S>))
        .route("/project/profile/{id}/", routing::get(projects::by_id::<S>))
        .route("/rust-ui/on", routing::get(opt_in::on))
        .route("/rust-ui/off", routing::get(opt_in::off))
        .route(
            &format!("{}/{{hash}}/{{name}}", assets::PREFIX),
            routing::get(assets::serve),
        )
}
