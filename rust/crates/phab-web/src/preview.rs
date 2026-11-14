//! Comment previews (`PhabricatorApplicationTransactionRemarkupPreviewController`):
//! the text of a form, rendered as Remarkup for the viewer, as its comment
//! will be.

use std::sync::Arc;

use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use phab_remarkup::Mode;

use crate::error::PageError;
use crate::response;
use crate::state::WebState;
use crate::viewer::WebUser;

/// Longer texts are not previewed: rendering them would cost more than a
/// preview is worth.
const MAX_BYTES: usize = 256 * 1024;

/// The form `phab.js` posts.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PreviewForm {
    #[serde(rename = "__csrf__")]
    csrf: String,
    #[serde(default)]
    text: String,
}

/// `POST /rust-ui/preview`: the HTML of `text`, without a page around it.
pub(crate) async fn render<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Form(form): Form<PreviewForm>,
) -> Result<Response, PageError> {
    if !viewer.is_valid_csrf_token(&form.csrf, state.now())? {
        return Err(PageError::BadRequest(
            "This form is out of date; reload the page.",
        ));
    }
    if form.text.len() > MAX_BYTES {
        return Err(PageError::BadRequest("This text is too long to preview."));
    }
    let markup = crate::remarkup::render(
        state.as_ref(),
        &viewer.user.viewer(),
        &form.text,
        Mode::Html,
    )
    .await?;
    let html = String::from_utf8_lossy(markup.as_bytes()).into_owned();
    Ok(response::html_response(StatusCode::OK, html))
}
