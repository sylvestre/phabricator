//! `/harbormaster/log/view/<id>/` (`HarbormasterBuildLogViewController`):
//! a build log, with links to its build. PHP's view loads the lines as
//! they are scrolled to and highlights ranges (`$12-14`); this page shows
//! the log, or its last lines with `?l=`, like the build page does.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_harbormaster::web::{self as harbormaster, Lookup};

use super::build::{LogView, line_limit, log_view};
use super::{Scope, method_error};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// Logs longer than this show their last lines unless `?l=0` asks for
/// all of them.
const WHOLE_LOG_BYTES: u64 = 1024 * 1024;

/// The lines shown of a longer log.
const LONG_LOG_LINES: i64 = 1000;

#[derive(Template)]
#[template(path = "hm_log_page.html")]
pub(crate) struct LogPage {
    shell: Shell,
    id: i64,
    build_id: Option<i64>,
    log: LogView,
}

/// `GET /harbormaster/log/view/<id>/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<LogPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let request = scope.request();
    let log = match harbormaster::log(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(log) => log,
        Lookup::Missing | Lookup::Denied => return Err(PageError::NotFound),
    };
    let has_limit = form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .any(|(key, _)| key == "l");
    let limit = if has_limit {
        line_limit(query.as_deref())?
    } else if log.byte_length > WHOLE_LOG_BYTES {
        LONG_LOG_LINES
    } else {
        0
    };
    let base_uri = format!("/harbormaster/log/view/{id}/");
    let (view, _) = log_view(state.as_ref(), &log, limit, &base_uri).await?;
    let target = harbormaster::targets_by_phid(&request, std::slice::from_ref(&log.target_phid))
        .await
        .map_err(method_error)?
        .into_iter()
        .next();
    let build_id = match target {
        Some(target) => harbormaster::builds_by_phid(&request, &[target.build_phid])
            .await
            .map_err(method_error)?
            .first()
            .map(|build| build.id),
        None => None,
    };
    let title = format!("Build Log {id}");
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(LogPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current),
        id,
        build_id,
        log: view,
    }))
}
