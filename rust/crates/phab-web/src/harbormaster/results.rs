//! The results builds report on a buildable: all its lint messages
//! (`/harbormaster/lint/<id>/`, `HarbormasterLintMessagesController`),
//! all its unit test results (`/harbormaster/unit/<id>/`,
//! `HarbormasterUnitMessageListController`) and one test result
//! (`/harbormaster/unit/view/<id>/`,
//! `HarbormasterUnitMessageViewController`).

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::{self as harbormaster, Buildable, Lookup};
use phab_core::Phid;

use super::buildable::{LintRow, builds_and_targets, lint};
use super::status::Status;
use super::unit::{self, UnitSummary};
use super::{Action, Scope, method_error};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

#[derive(Template)]
#[template(path = "hm_lint_page.html")]
pub(crate) struct LintPage {
    shell: Shell,
    id: i64,
    lint: Vec<LintRow>,
}

#[derive(Template)]
#[template(path = "hm_unit_page.html")]
pub(crate) struct UnitPage {
    shell: Shell,
    id: i64,
    unit: UnitSummary,
}

#[derive(Template)]
#[template(path = "hm_unit_view.html")]
pub(crate) struct UnitViewPage {
    shell: Shell,
    id: i64,
    name: String,
    status: Status,
    buildable_id: i64,
    run_at: String,
    details_html: String,
    is_text: bool,
    actions: Vec<Action>,
}

/// The buildable `B<id>` the viewer sees, and the targets of its builds'
/// current runs.
async fn buildable_targets(scope: &Scope, id: i64) -> Result<(Buildable, Vec<Phid>), PageError> {
    let request = scope.request();
    let buildable = match harbormaster::buildable(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(buildable) => buildable,
        Lookup::Missing | Lookup::Denied => return Err(PageError::NotFound),
    };
    let (_, targets) = builds_and_targets(&request, &buildable).await?;
    Ok((
        buildable,
        targets.into_iter().map(|target| target.phid).collect(),
    ))
}

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// `GET /harbormaster/lint/<id>/`.
pub(crate) async fn lint_page<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<LintPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let (_, targets) = buildable_targets(&scope, id).await?;
    let lint = lint(state.as_ref(), &targets, None).await?;
    let title = format!("B{id} Lint");
    Ok(Page(LintPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
        id,
        lint,
    }))
}

/// `GET /harbormaster/unit/<id>/`.
pub(crate) async fn unit_page<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<UnitPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let (_, targets) = buildable_targets(&scope, id).await?;
    let messages = harbormaster::unit_messages(state.database(), &targets).await?;
    let unit = unit::summary(state.as_ref(), &scope.viewer(), id, messages, None).await?;
    let title = format!("B{id} Unit Tests");
    Ok(Page(UnitPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
        id,
        unit,
    }))
}

/// `GET /harbormaster/unit/view/<id>/`: the result, if the viewer sees
/// its target.
pub(crate) async fn unit_view<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<UnitViewPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let request = scope.request();
    let message = harbormaster::unit_message(state.database(), id)
        .await?
        .ok_or(PageError::NotFound)?;
    let target =
        harbormaster::targets_by_phid(&request, std::slice::from_ref(&message.target_phid))
            .await
            .map_err(method_error)?
            .pop()
            .ok_or(PageError::NotFound)?;
    let build = harbormaster::builds_by_phid(&request, std::slice::from_ref(&target.build_phid))
        .await
        .map_err(method_error)?
        .pop()
        .ok_or_else(|| PageError::Fallback("a visible target without its build"))?;
    let buildable =
        harbormaster::buildables_by_phid(&request, std::slice::from_ref(&build.buildable_phid))
            .await
            .map_err(method_error)?
            .pop()
            .ok_or_else(|| PageError::Fallback("a visible build without its buildable"))?;
    let name = unit::display_name(&message)?;
    let (details_html, is_text) =
        unit::full_details(state.as_ref(), &scope.viewer(), &message).await?;
    let actions = vec![Action {
        name: "View Build",
        icon: "fa-wrench",
        uri: format!("/harbormaster/build/{}/", build.id),
        enabled: true,
    }];
    let title = format!("{name} · B{}", buildable.id);
    Ok(Page(UnitViewPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current(&uri)),
        id,
        status: unit::result_status(&message.result),
        name,
        buildable_id: buildable.id,
        run_at: time::format_utc(message.date_created),
        details_html,
        is_text,
        actions,
    }))
}
