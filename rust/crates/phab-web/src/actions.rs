//! Review actions on a revision (W6): comment, accept, request changes and
//! the others, sent to PHP's `differential.revision.edit` as the user.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use phab_policy::application;
use serde_json::{Value, json};

use crate::error::PageError;
use crate::revision;
use crate::state::WebState;
use crate::viewer::WebUser;

/// The actions the form offers: the `ACTIONKEY`s of the
/// `DifferentialRevisionActionTransaction` classes, and their labels.
pub(crate) const ACTIONS: &[(&str, &str)] = &[
    ("accept", "Accept Revision"),
    ("reject", "Request Changes"),
    ("resign", "Resign as Reviewer"),
    ("request-review", "Request Review"),
    ("plan-changes", "Plan Changes"),
    ("commandeer", "Commandeer Revision"),
    ("abandon", "Abandon Revision"),
    ("reclaim", "Reclaim Revision"),
    ("reopen", "Reopen Revision"),
];

/// The form of the revision page.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ActionForm {
    #[serde(rename = "__csrf__")]
    csrf: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    comment: String,
}

/// The `transactions` of `differential.revision.edit`: the action, then
/// the comment, like the old interface's "Add Comment" form.
fn transactions(form: &ActionForm) -> Option<Vec<Value>> {
    let mut transactions = Vec::new();
    if !form.action.is_empty() {
        if !ACTIONS.iter().any(|(key, _)| *key == form.action) {
            return None;
        }
        transactions.push(json!({"type": form.action, "value": true}));
    }
    if !form.comment.trim().is_empty() {
        transactions.push(json!({"type": "comment", "value": form.comment}));
    }
    Some(transactions)
}

/// `POST /D{id}/action`: PHP checks the action is allowed and applies it.
pub(crate) async fn submit<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(monogram): Path<String>,
    Form(form): Form<ActionForm>,
) -> Result<Response, PageError> {
    viewer
        .require_application(&*state, application::DIFFERENTIAL)
        .await?;
    let id = revision::parse_monogram(&monogram).ok_or(PageError::NotFound)?;
    if !viewer.is_valid_csrf_token(&form.csrf, state.now())? {
        return Err(PageError::BadRequest(
            "Your session expired; reload the page and try again.",
        ));
    }
    let transactions =
        transactions(&form).ok_or(PageError::BadRequest("This action does not exist."))?;
    let back = format!("/D{id}");
    if !transactions.is_empty() {
        let php = state
            .php()
            .ok_or(PageError::Fallback("PHP's Conduit API is not configured"))?;
        let params = json!({"objectIdentifier": format!("D{id}"), "transactions": transactions});
        php.call_as_user(
            "differential.revision.edit",
            &params,
            viewer.session_token(),
            &viewer.csrf_token,
        )
        .await
        .map_err(PageError::Conduit)?;
    }
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::from_str(&back) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(action: &str, comment: &str) -> ActionForm {
        ActionForm {
            csrf: String::new(),
            action: action.to_owned(),
            comment: comment.to_owned(),
        }
    }

    #[test]
    fn actions_and_comments_become_transactions() {
        assert_eq!(
            transactions(&form("accept", "LGTM")),
            Some(vec![
                json!({"type": "accept", "value": true}),
                json!({"type": "comment", "value": "LGTM"})
            ])
        );
        assert_eq!(transactions(&form("", "  ")), Some(vec![]));
        assert_eq!(transactions(&form("destroy", "")), None);
    }
}
