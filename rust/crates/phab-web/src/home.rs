//! The home page. A placeholder until the review queue is ported (W5).

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::Uri;

use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

#[derive(Template)]
#[template(path = "home.html")]
pub(crate) struct Home {
    shell: Shell,
}

/// `GET /`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<Home>, PageError> {
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(Home {
        shell: Shell::for_user(state.config(), &viewer, "Home", current),
    }))
}
