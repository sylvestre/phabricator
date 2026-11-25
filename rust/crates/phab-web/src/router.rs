//! The web routes.

use std::sync::Arc;

use axum::{Router, routing};

use crate::state::WebState;
use crate::{
    actions, assets, changeset_view, comment_edit, dashboards, datasource, diff_create, diff_page,
    diffusion, favorites, feed, files, flags, harbormaster, herald, inline_edit, login,
    notifications, opt_in, people, people_lists, preview, project_edit, project_manage,
    project_members, projects, queue, revision, revision_edit, revision_pages, settings,
    subscriptions, transactions, typeahead, web_edit,
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
            "/project/manage/{id}/",
            routing::get(project_manage::manage::<S>),
        )
        .route(
            "/project/filter/{filter}/",
            routing::get(projects::filter::<S>),
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
        .route(
            "/portal/view/{id}/view/{item}/",
            routing::get(dashboards::portal_item::<S>),
        )
        .route("/portal/", routing::get(dashboards::portal_list::<S>))
        .route(
            "/portal/query/{key}/",
            routing::get(dashboards::portal_query::<S>),
        )
        .route(
            "/dashboard/console/",
            routing::get(dashboards::console::<S>),
        )
        .route(
            "/portal/view/{id}/configure/",
            routing::get(dashboards::configure_portal::<S>),
        )
        .route(
            "/home/menu/configure/",
            routing::get(dashboards::configure_home::<S>),
        )
        .route(
            "/favorites/menu/configure/",
            routing::get(dashboards::configure_favorites::<S>),
        )
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
            "/harbormaster/build/{first}/{second}/",
            routing::get(harbormaster::build_run_or_command::<S>)
                .post(harbormaster::build_run_or_command::<S>),
        )
        .route(
            "/harbormaster/build/{command}/{id}/{via}/",
            routing::get(harbormaster::build_command_via::<S>)
                .post(harbormaster::build_command_via::<S>),
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
            "/harbormaster/lint/{id}/",
            routing::get(harbormaster::lint_messages::<S>),
        )
        .route(
            "/harbormaster/unit/{id}/",
            routing::get(harbormaster::unit_messages::<S>),
        )
        .route(
            "/harbormaster/unit/view/{id}/",
            routing::get(harbormaster::unit_message::<S>),
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

/// Differential: the queue, revisions (whose path is the monogram route
/// other objects share), diffs and changesets.
fn differential<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/differential/", routing::get(queue::show::<S>))
        .route("/{monogram}", routing::get(revision::by_monogram::<S>))
        .route("/{monogram}/action", routing::post(actions::submit::<S>))
        .route("/{monogram}/new/", routing::get(revision::show_new::<S>))
        .route(
            "/differential/diff/{id}/",
            routing::get(diff_page::show::<S>),
        )
        .route(
            "/differential/diff/{id}/changesets/",
            routing::get(diff_page::changesets::<S>),
        )
        .route(
            "/differential/diff/{id}/changesets/query/{key}/",
            routing::get(diff_page::changeset_query::<S>),
        )
        .route(
            "/differential/changeset/",
            routing::get(changeset_view::view::<S>).post(changeset_view::view::<S>),
        )
        .route(
            "/differential/revision/closedetails/{phid}/",
            routing::get(revision_pages::close_details::<S>),
        )
        .route(
            "/differential/revision/inlines/{id}/",
            routing::get(revision_pages::inlines::<S>),
        )
        .route(
            "/differential/revision/paths/{id}/",
            routing::get(revision_pages::paths::<S>),
        )
        .route(
            "/differential/comment/inline/edit/{id}/",
            routing::any(inline_edit::edit::<S>),
        )
}

/// The pages of a transaction and its comment.
fn transaction_routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/transactions/detail/{phid}/",
            routing::get(transactions::detail::<S>),
        )
        .route(
            "/transactions/history/{phid}/",
            routing::get(transactions::history::<S>),
        )
        .route(
            "/transactions/raw/{phid}/",
            routing::get(transactions::raw::<S>),
        )
        .route(
            "/transactions/quote/{phid}/",
            routing::get(transactions::quote::<S>).post(transactions::quote::<S>),
        )
        .route(
            "/transactions/edit/{phid}/",
            routing::get(transactions::edit::<S>).post(comment_edit::edit::<S>),
        )
        .route(
            "/transactions/remove/{phid}/",
            routing::get(transactions::remove::<S>).post(comment_edit::remove::<S>),
        )
}

/// Herald: rules, transcripts, the read side of its forms and webhooks.
fn herald_routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
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
        .route(
            "/herald/disable/{id}/{action}/",
            routing::get(herald::disable::<S>).post(herald::disable::<S>),
        )
        .route(
            "/herald/new/",
            routing::get(herald::new_rule::<S>).post(herald::new_rule::<S>),
        )
        .route(
            "/herald/create/",
            routing::get(herald::new_rule::<S>).post(herald::new_rule::<S>),
        )
        .route(
            "/herald/edit/",
            routing::get(herald::rule_create::<S>).post(herald::rule_create::<S>),
        )
        .route(
            "/herald/edit/{id}/",
            routing::get(herald::rule_edit::<S>).post(herald::rule_edit::<S>),
        )
        .route(
            "/herald/test/",
            routing::get(herald::test_console::<S>).post(herald::test_console::<S>),
        )
        .route("/herald/webhook/", routing::get(herald::webhook_list::<S>))
        .route(
            "/herald/webhook/query/{key}/",
            routing::get(herald::webhook_query::<S>),
        )
        .route(
            "/herald/webhook/view/{id}/",
            routing::get(herald::webhook_view::<S>),
        )
        .route(
            "/herald/webhook/view/{id}/request/{request}/",
            routing::get(herald::webhook_request::<S>),
        )
        .route(
            "/herald/webhook/test/{id}/",
            routing::get(herald::webhook_test::<S>).post(herald::webhook_test::<S>),
        )
        .route(
            "/herald/webhook/key/{action}/{id}/",
            routing::get(herald::webhook_key::<S>).post(herald::webhook_key::<S>),
        )
}

/// The pages, the static files and the interface switch. nginx decides
/// which of them reach the service (`rust/nginx/rust-web.conf`).
pub fn router<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .merge(crate::system::routes::<S>())
        .merge(differential::<S>())
        .merge(web_edit::routes::<S>())
        .merge(revision_edit::routes::<S>())
        .merge(diff_create::routes::<S>())
        .merge(project_edit::routes::<S>())
        .merge(project_members::routes::<S>())
        .merge(dashboards::edit_routes::<S>())
        .merge(people_and_activity::<S>())
        .merge(file_routes::<S>())
        .route("/flag/", routing::get(flags::list::<S>))
        .route("/flag/query/{key}/", routing::get(flags::query::<S>))
        .route(
            "/subscriptions/list/{phid}/",
            routing::get(subscriptions::list::<S>),
        )
        .merge(transaction_routes::<S>())
        .merge(harbormaster_routes::<S>())
        .merge(herald_routes::<S>())
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
