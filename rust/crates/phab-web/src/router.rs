//! The web routes.

use std::sync::Arc;

use axum::{Router, routing};

use crate::state::WebState;
use crate::{actions, assets, feed, opt_in, people, preview, projects, queue, revision};

/// The pages, the static files and the interface switch. nginx decides
/// which of them reach the service (`rust/nginx/rust-web.conf`).
pub fn router<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/", routing::get(queue::show::<S>))
        .route("/differential/", routing::get(queue::show::<S>))
        .route("/{monogram}", routing::get(revision::show::<S>))
        .route("/{monogram}/action", routing::post(actions::submit::<S>))
        .route("/p/{username}/", routing::get(people::show::<S>))
        .route("/tag/{slug}/", routing::get(projects::by_slug::<S>))
        .route("/project/view/{id}/", routing::get(projects::by_id::<S>))
        .route("/project/profile/{id}/", routing::get(projects::by_id::<S>))
        .route("/feed/", routing::get(feed::show::<S>))
        .route("/rust-ui/on", routing::get(opt_in::on))
        .route("/rust-ui/off", routing::get(opt_in::off))
        .route("/rust-ui/preview", routing::post(preview::render::<S>))
        .route(
            &format!("{}/{{hash}}/{{name}}", assets::PREFIX),
            routing::get(assets::serve),
        )
}
