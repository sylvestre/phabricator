//! Review actions on a revision (W6): comment, accept, request changes and
//! the others, add reviewers and subscribers, sent to PHP's
//! `differential.revision.edit` as the user.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use phab_app_project as project;
use phab_core::Phid;
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
    /// Users and `#projects` to add as reviewers; `name!` is blocking.
    #[serde(default)]
    reviewers: String,
    /// Users and `#projects` to add as subscribers.
    #[serde(default)]
    subscribers: String,
}

/// A name typed in the form.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entered {
    /// The username or the project hashtag, without `@`, `#` or `!`.
    name: String,
    is_project: bool,
    is_blocking: bool,
}

/// The names of a field, separated by commas or spaces.
fn names(field: &str) -> Vec<Entered> {
    field
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|word| !word.is_empty())
        .filter_map(|word| {
            let (word, is_blocking) = match word.strip_suffix('!') {
                Some(word) => (word, true),
                None => (word, false),
            };
            let (name, is_project) = match word.strip_prefix('#') {
                Some(slug) => (slug.to_lowercase(), true),
                None => (word.trim_start_matches('@').to_owned(), false),
            };
            (!name.is_empty()).then_some(Entered {
                name,
                is_project,
                is_blocking,
            })
        })
        .collect()
}

/// The PHIDs of `names`, as `viewer` may see them: an unknown or hidden
/// name is an error naming it, as PHP's datasource refuses it.
async fn resolve<S: WebState>(
    state: &S,
    viewer: &WebUser,
    names: &[Entered],
) -> Result<Vec<(Phid, bool)>, PageError> {
    let usernames: Vec<&str> = names
        .iter()
        .filter(|name| !name.is_project)
        .map(|name| name.name.as_str())
        .collect();
    let users = phab_auth::user::load_users_by_username(state.database(), &usernames).await?;
    let slugs: Vec<String> = names
        .iter()
        .filter(|name| name.is_project)
        .map(|name| name.name.clone())
        .collect();
    let found = project::storage::load_by_slug(state.database(), &slugs).await?;
    let (visible, _) = project::policy::visible_projects(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        found.iter().map(|(_, project)| project.clone()).collect(),
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    names
        .iter()
        .map(|name| {
            let phid = if name.is_project {
                found
                    .iter()
                    .find(|(slug, _)| *slug == name.name)
                    .map(|(_, project)| &project.phid)
                    .filter(|phid| visible.iter().any(|project| project.phid == **phid))
            } else {
                users
                    .iter()
                    .find(|user| user.user_name.eq_ignore_ascii_case(&name.name))
                    .filter(|user| !user.is_disabled)
                    .map(|user| &user.phid)
            };
            let prefix = if name.is_project { "#" } else { "" };
            phid.map(|phid| (phid.clone(), name.is_blocking))
                .ok_or_else(|| {
                    PageError::Invalid(format!(
                        "No user or project is named {prefix}{}.",
                        name.name
                    ))
                })
        })
        .collect()
}

/// The `transactions` of `differential.revision.edit`: the action, the
/// reviewers and subscribers, then the comment, like the old interface's
/// "Add Comment" form.
fn transactions(
    form: &ActionForm,
    reviewers: &[(Phid, bool)],
    subscribers: &[(Phid, bool)],
) -> Option<Vec<Value>> {
    let mut transactions = Vec::new();
    if !form.action.is_empty() {
        if !ACTIONS.iter().any(|(key, _)| *key == form.action) {
            return None;
        }
        transactions.push(json!({"type": form.action, "value": true}));
    }
    if !reviewers.is_empty() {
        let value: Vec<String> = reviewers
            .iter()
            .map(|(phid, is_blocking)| {
                if *is_blocking {
                    format!("blocking({phid})")
                } else {
                    phid.to_string()
                }
            })
            .collect();
        transactions.push(json!({"type": "reviewers.add", "value": value}));
    }
    if !subscribers.is_empty() {
        let value: Vec<&str> = subscribers.iter().map(|(phid, _)| phid.as_str()).collect();
        transactions.push(json!({"type": "subscribers.add", "value": value}));
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
    let reviewers = resolve(state.as_ref(), &viewer, &names(&form.reviewers)).await?;
    let subscribers = resolve(state.as_ref(), &viewer, &names(&form.subscribers)).await?;
    let transactions = transactions(&form, &reviewers, &subscribers)
        .ok_or(PageError::BadRequest("This action does not exist."))?;
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
            reviewers: String::new(),
            subscribers: String::new(),
        }
    }

    #[test]
    fn actions_and_comments_become_transactions() {
        assert_eq!(
            transactions(&form("accept", "LGTM"), &[], &[]),
            Some(vec![
                json!({"type": "accept", "value": true}),
                json!({"type": "comment", "value": "LGTM"})
            ])
        );
        assert_eq!(transactions(&form("", "  "), &[], &[]), Some(vec![]));
        assert_eq!(transactions(&form("destroy", ""), &[], &[]), None);
        assert_eq!(
            transactions(
                &form("", "Over to you"),
                &[
                    (Phid::new("PHID-USER-a"), false),
                    (Phid::new("PHID-PROJ-b"), true)
                ],
                &[(Phid::new("PHID-USER-c"), false)],
            ),
            Some(vec![
                json!({"type": "reviewers.add", "value": ["PHID-USER-a", "blocking(PHID-PROJ-b)"]}),
                json!({"type": "subscribers.add", "value": ["PHID-USER-c"]}),
                json!({"type": "comment", "value": "Over to you"}),
            ])
        );
    }

    #[test]
    fn names_are_users_and_projects() {
        let name = |name: &str, is_project, is_blocking| Entered {
            name: name.to_owned(),
            is_project,
            is_blocking,
        };
        assert_eq!(
            names(" alice, @bob  #Build-Team! carol!,,"),
            vec![
                name("alice", false, false),
                name("bob", false, false),
                name("build-team", true, true),
                name("carol", false, true),
            ]
        );
        assert_eq!(names("# ! @"), vec![]);
    }
}
