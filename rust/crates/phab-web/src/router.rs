//! The web routes.

use std::sync::Arc;

use axum::{Router, routing};

use crate::state::WebState;
use crate::{
    actions, assets, dashboards, datasource, diffusion, favorites, feed, files, flags,
    harbormaster, herald, login, notifications, opt_in, people, people_lists, preview,
    project_edit, projects, queue, revision, settings, subscriptions, transactions, typeahead,
    web_edit,
};

/// The login, registration and account linking pages (W7).
fn login_routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/auth/start/", routing::any(login::start::<S>))
        .route("/login/", routing::any(login::start::<S>))
        .route("/auth/loggedout/", routing::any(login::logged_out::<S>))
        .route(
            "/auth/login/{pkey}/",
            routing::any(login::provider_login::<S>),
        )
        .route(
            "/auth/login/{pkey}/{extra}/",
            routing::any(|| async { crate::fallback_response("a login URI with extra data") }),
        )
        .route("/auth/validate/", routing::any(login::validate::<S>))
        .route("/auth/finish/", routing::any(login::finish::<S>))
        .route("/auth/link/{id}/", routing::any(login::link::<S>))
        .route("/auth/refresh/{id}/", routing::any(login::refresh::<S>))
        .route("/auth/register/{akey}/", routing::any(login::register::<S>))
        .route(
            "/auth/register/",
            routing::any(|| async {
                crate::fallback_response("a registration without an account key")
            }),
        )
        .route(
            "/auth/confirmlink/{akey}/",
            routing::any(login::confirm_link::<S>),
        )
        .route("/logout/", routing::any(login::logout::<S>))
}

/// Profiles, projects, the feed and notifications.
fn people_and_activity<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/p/{username}/", routing::get(people::show::<S>))
        .route(
            "/people/revisions/{id}/",
            routing::get(people::revisions::<S>),
        )
        .route("/people/", routing::get(people_lists::list::<S>))
        .route(
            "/people/query/{key}/",
            routing::get(people_lists::query::<S>),
        )
        .route(
            "/people/manage/{id}/",
            routing::get(people_lists::manage::<S>),
        )
        .route("/tag/{slug}/", routing::get(projects::by_slug::<S>))
        .route("/project/", routing::get(projects::list::<S>))
        .route("/project/query/{key}/", routing::get(projects::query::<S>))
        .route("/project/view/{id}/", routing::get(projects::by_id::<S>))
        .route("/project/profile/{id}/", routing::get(projects::by_id::<S>))
        .route(
            "/project/members/{id}/",
            routing::get(projects::members::<S>),
        )
        .route(
            "/project/subprojects/{id}/",
            routing::get(projects::subprojects::<S>),
        )
        .route("/favorites/", routing::get(favorites::show::<S>))
        .route("/feed/", routing::get(feed::show::<S>))
        .route("/feed/query/{key}/", routing::get(feed::query::<S>))
        .route("/feed/{key}/", routing::get(feed::detail::<S>))
        .route("/notification/", routing::get(notifications::show::<S>))
        .route(
            "/notification/query/{key}/",
            routing::get(notifications::query::<S>),
        )
}

/// Dashboards, portals and their panels.
fn dashboards<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/dashboard/view/{id}/",
            routing::get(dashboards::dashboard::<S>),
        )
        .route("/portal/view/{id}/", routing::get(dashboards::portal::<S>))
        .route("/dashboard/", routing::get(dashboards::dashboard_list::<S>))
        .route(
            "/dashboard/query/{key}/",
            routing::get(dashboards::dashboard_query::<S>),
        )
        .route(
            "/dashboard/panel/",
            routing::get(dashboards::panel_list::<S>),
        )
        .route(
            "/dashboard/panel/query/{key}/",
            routing::get(dashboards::panel_query::<S>),
        )
}

/// Harbormaster: lists, builds, plans, steps and logs.
fn harbormaster_routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/harbormaster/",
            routing::get(harbormaster::buildable_list::<S>),
        )
        .route(
            "/harbormaster/build/{id}/",
            routing::get(harbormaster::build::<S>),
        )
        .route(
            "/harbormaster/build/{id}/{generation}/",
            routing::get(harbormaster::build_run::<S>),
        )
        .route(
            "/harbormaster/plan/",
            routing::get(harbormaster::plan_list::<S>),
        )
        .route(
            "/harbormaster/plan/query/{key}/",
            routing::get(harbormaster::plan_query::<S>),
        )
        .route(
            "/harbormaster/build/",
            routing::get(harbormaster::build_list::<S>),
        )
        .route(
            "/harbormaster/build/query/{key}/",
            routing::get(harbormaster::build_query::<S>),
        )
        .route(
            "/harbormaster/log/view/{id}/",
            routing::get(harbormaster::log::<S>),
        )
        .route(
            "/harbormaster/step/view/{id}/",
            routing::get(harbormaster::step::<S>),
        )
        .route(
            "/harbormaster/plan/{id}/",
            routing::get(harbormaster::plan::<S>),
        )
}

/// Files: pages, lists and data.
fn file_routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/file/", routing::get(files::list::<S>))
        .route("/file/query/{key}/", routing::get(files::query::<S>))
        .route("/file/info/{phid}/", routing::get(files::info::<S>))
        .route("/file/view/{id}/", routing::get(files::view::<S>))
        .route("/file/data/{*rest}", routing::any(files::data::<S>))
        .route("/file/download/{*rest}", routing::any(files::download::<S>))
}

/// The pages, the static files and the interface switch. nginx decides
/// which of them reach the service (`rust/nginx/rust-web.conf`).
pub fn router<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .merge(crate::system::routes::<S>())
        .route("/differential/", routing::get(queue::show::<S>))
        .route("/{monogram}", routing::get(revision::by_monogram::<S>))
        .route("/{monogram}/action", routing::post(actions::submit::<S>))
        .merge(web_edit::routes::<S>())
        .merge(project_edit::routes::<S>())
        .merge(people_and_activity::<S>())
        .merge(file_routes::<S>())
        .route("/flag/", routing::get(flags::list::<S>))
        .route("/flag/query/{key}/", routing::get(flags::query::<S>))
        .route(
            "/subscriptions/list/{phid}/",
            routing::get(subscriptions::list::<S>),
        )
        .route(
            "/transactions/detail/{phid}/",
            routing::get(transactions::detail::<S>),
        )
        .route(
            "/transactions/history/{phid}/",
            routing::get(transactions::history::<S>),
        )
        .merge(harbormaster_routes::<S>())
        .route("/herald/", routing::get(herald::rule_list::<S>))
        .route(
            "/herald/query/{key}/",
            routing::get(herald::rule_query::<S>),
        )
        .route(
            "/herald/transcript/",
            routing::get(herald::transcript_list::<S>),
        )
        .route(
            "/herald/transcript/query/{key}/",
            routing::get(herald::transcript_list::<S>),
        )
        .route(
            "/herald/transcript/{id}/",
            routing::get(herald::transcript::<S>),
        )
        .route(
            "/herald/transcript/{id}/{view}/",
            routing::get(herald::transcript_view::<S>),
        )
        .merge(dashboards::<S>())
        .route(
            "/source/{short}/",
            routing::get(diffusion::source_root::<S>),
        )
        .route(
            "/source/{short}/{*rest}",
            routing::get(diffusion::source::<S>).post(diffusion::source_post::<S>),
        )
        .route(
            "/diffusion/{identifier}/",
            routing::get(diffusion::diffusion_root::<S>),
        )
        .route("/diffusion/", routing::get(diffusion::list::<S>))
        .route(
            "/diffusion/query/{key}/",
            routing::get(diffusion::list_query::<S>),
        )
        .route(
            "/diffusion/{identifier}/{*rest}",
            routing::get(diffusion::diffusion::<S>).post(diffusion::diffusion_post::<S>),
        )
        .merge(settings::routes::<S>())
        .merge(login_routes::<S>())
        .route("/rust-ui/on", routing::get(opt_in::on))
        .route("/rust-ui/off", routing::get(opt_in::off))
        .route("/rust-ui/preview", routing::post(preview::render::<S>))
        .route("/rust-ui/typeahead", routing::get(typeahead::suggest::<S>))
        .route(
            "/typeahead/class/{class}/",
            routing::get(datasource::class::<S>).post(datasource::class::<S>),
        )
        .route(
            &format!("{}/{{hash}}/{{name}}", assets::PREFIX),
            routing::get(assets::serve),
        )
        // PHP's workflows post to pages Rust only shows: PHP answers them.
        .method_not_allowed_fallback(|| async {
            crate::fallback_response("a request method not ported")
        })
}
