//! Review actions on a revision (W6): comment, accept, request changes and
//! the others, add or remove reviewers and subscribers. They are a post of
//! PHP's comment form ([`web_edit`]), written by Rust when it can and sent
//! to PHP as the user otherwise; with inline comments, a call to PHP's
//! `differential.revision.edit` as the user.

use std::sync::Arc;

use axum::Form;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use phab_app_differential::storage;
use phab_app_project as project;
use phab_core::Phid;
use phab_db::edges;
use phab_policy::application;
use serde_json::{Value, json};

use crate::error::PageError;
use crate::revision;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit::{self, CommentForm, FormAction};

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
    ("close", "Close Revision"),
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
    /// A reviewer to remove, by PHID (the "×" next to them).
    #[serde(default)]
    remove_reviewer: String,
    /// A subscriber to remove, by PHID.
    #[serde(default)]
    remove_subscriber: String,
    /// The inline comments written on the page, as a JSON list
    /// ([`InlineDraft`]); `phab.js` keeps them until the form is sent.
    #[serde(default)]
    inlines: String,
}

/// At most this many inline comments in one edit.
const MAX_INLINES: usize = 500;

/// An inline comment of the form: a new one on lines of a file, or a reply
/// to a published one. They become the `inline` transactions of the edit
/// (`DifferentialRevisionEditEngine`'s Conduit documentation).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
enum InlineDraft {
    /// A reply, on the lines of the comment it replies to.
    Reply {
        #[serde(rename = "replyToCommentPHID")]
        reply_to_comment_phid: String,
        content: String,
    },
    /// A comment on lines of a file of the diff.
    #[serde(rename_all = "camelCase")]
    New {
        path: String,
        line: i64,
        #[serde(default = "one")]
        length: i64,
        #[serde(default = "yes")]
        is_new_file: bool,
        diff: Option<i64>,
        content: String,
    },
}

fn one() -> i64 {
    1
}

fn yes() -> bool {
    true
}

impl InlineDraft {
    /// The value of the `inline` transaction, if the comment is complete.
    fn value(&self) -> Option<Value> {
        match self {
            Self::Reply {
                reply_to_comment_phid,
                content,
            } => (reply_to_comment_phid.starts_with("PHID-XCMT-") && !content.trim().is_empty())
                .then(|| json!({"replyToCommentPHID": reply_to_comment_phid, "content": content})),
            Self::New {
                path,
                line,
                length,
                is_new_file,
                diff,
                content,
            } => {
                let complete =
                    !path.is_empty() && *line >= 1 && *length >= 1 && !content.trim().is_empty();
                complete.then(|| {
                    let mut value = json!({
                        "path": path,
                        "line": line,
                        "length": length,
                        "isNewFile": is_new_file,
                        "content": content,
                    });
                    if let Some(diff) = diff {
                        value["diff"] = json!(diff);
                    }
                    value
                })
            }
        }
    }
}

/// The `inline` transactions of the form's `inlines`; an error says what
/// is wrong with them.
fn inline_transactions(inlines: &str) -> Result<Vec<Value>, &'static str> {
    if inlines.trim().is_empty() {
        return Ok(Vec::new());
    }
    let drafts: Vec<InlineDraft> =
        serde_json::from_str(inlines).map_err(|_| "The inline comments could not be read.")?;
    if drafts.len() > MAX_INLINES {
        return Err("There are too many inline comments to publish at once.");
    }
    drafts
        .iter()
        .map(|draft| {
            draft
                .value()
                .map(|value| json!({"type": "inline", "value": value}))
                .ok_or("An inline comment is empty or has no place in the diff.")
        })
        .collect()
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

/// Names typed in the form: their PHIDs, and whether they are blocking.
type Resolved = [(Phid, bool)];

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
/// reviewers and subscribers, the inline comments, then the comment, like
/// the old interface's "Add Comment" form.
fn transactions(
    form: &ActionForm,
    reviewers: &[(Phid, bool)],
    subscribers: &[(Phid, bool)],
    inlines: Vec<Value>,
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
    for (kind, phid) in [
        ("reviewers.remove", &form.remove_reviewer),
        ("subscribers.remove", &form.remove_subscriber),
    ] {
        if phid.is_empty() {
            continue;
        }
        // Reviewers and subscribers are users and projects.
        let valid = (phid.starts_with("PHID-USER-") || phid.starts_with("PHID-PROJ-"))
            && phid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            return None;
        }
        transactions.push(json!({"type": kind, "value": [phid]}));
    }
    transactions.extend(inlines);
    if !form.comment.trim().is_empty() {
        transactions.push(json!({"type": "comment", "value": form.comment}));
    }
    Some(transactions)
}

/// The PHID in a reviewer token, `PHID` or `blocking(PHID)`.
fn token_phid(token: &str) -> &str {
    token
        .strip_prefix("blocking(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(token)
}

/// `list` with `added` (tokens replacing those of the same PHID) and
/// without the tokens of `removed`, as the tokenizer of PHP's form leaves
/// it.
fn edited_list(list: &[String], added: &[String], removed: &str) -> Vec<String> {
    let mut edited = list.to_vec();
    for token in added {
        let phid = token_phid(token);
        match edited.iter_mut().find(|known| token_phid(known) == phid) {
            Some(known) => known.clone_from(token),
            None => edited.push(token.clone()),
        }
    }
    edited.retain(|token| removed.is_empty() || token_phid(token) != removed);
    edited
}

/// The action of PHP's form which changes the list of `key` from `list`,
/// if it changes.
fn list_action(key: &str, list: &[String], edited: &[String]) -> Option<FormAction> {
    (edited != list).then(|| FormAction {
        kind: key.to_owned(),
        value: json!(edited),
        initial: json!(list),
    })
}

/// The post of PHP's comment form which does what `form` asks, `None` when
/// it asks for nothing: the action, the reviewer and subscriber changes,
/// then the comment.
async fn comment_form<S: WebState>(
    state: &S,
    form: &ActionForm,
    (reviewers, subscribers): (&Resolved, &Resolved),
    id: i64,
) -> Result<Option<CommentForm>, PageError> {
    let database = state.database();
    // PHP says what is wrong with a revision which doesn't exist.
    let revision = storage::load_revisions_by_id(database, &[id]).await?.pop();
    let mut actions = Vec::new();
    if !form.action.is_empty() {
        actions.push(FormAction {
            kind: form.action.clone(),
            value: json!(true),
            initial: Value::Null,
        });
    }
    // getReviewerPHIDsForEdit().
    let current: Vec<String> = revision
        .iter()
        .flat_map(|revision| &revision.reviewers)
        .map(|reviewer| {
            if reviewer.status == "blocking" {
                format!("blocking({})", reviewer.phid)
            } else {
                reviewer.phid.to_string()
            }
        })
        .collect();
    let added: Vec<String> = reviewers
        .iter()
        .map(|(phid, is_blocking)| {
            if *is_blocking {
                format!("blocking({phid})")
            } else {
                phid.to_string()
            }
        })
        .collect();
    let edited = edited_list(&current, &added, &form.remove_reviewer);
    actions.extend(list_action("reviewers", &current, &edited));
    let phids: Vec<Phid> = revision
        .iter()
        .map(|revision| revision.phid.clone())
        .collect();
    let current: Vec<String> =
        edges::load_destination_phids(database, &phids, &[edges::OBJECT_HAS_SUBSCRIBER], None)
            .await?
            .iter()
            .map(ToString::to_string)
            .collect();
    let added: Vec<String> = subscribers
        .iter()
        .map(|(phid, _)| phid.to_string())
        .collect();
    let edited = edited_list(&current, &added, &form.remove_subscriber);
    actions.extend(list_action("subscriberPHIDs", &current, &edited));
    let comment = if form.comment.trim().is_empty() {
        String::new()
    } else {
        form.comment.clone()
    };
    if actions.is_empty() && comment.is_empty() {
        return Ok(None);
    }
    Ok(Some(CommentForm {
        actions,
        comment: Some(comment),
        // The form's `{}`.
        metadata: json!([]),
    }))
}

/// A redirect to `back`.
fn see_other(back: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::from_str(back) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

/// Sends `form` to PHP's comment form as the viewer: PHP saves it, or
/// shows why not, as its own page would.
async fn post_to_php<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
    form: &CommentForm,
) -> Result<Response, PageError> {
    let php = state
        .php()
        .ok_or(PageError::Fallback("PHP's Conduit API is not configured"))?;
    let answer = php
        .post_form_as_user(
            &format!("/differential/revision/edit/{id}/comment/"),
            &form.php_fields(&viewer.csrf_token),
            viewer.session_token(),
        )
        .await
        .map_err(PageError::Conduit)?;
    if (300..400).contains(&answer.status) {
        return Ok(see_other(&format!("/D{id}")));
    }
    // PHP's dialog. Not 502 or 504, which nginx would replay.
    let status = match StatusCode::from_u16(answer.status) {
        Ok(StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT) | Err(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        Ok(status) => status,
    };
    let mut response = Response::new(Body::from(answer.body));
    *response.status_mut() = status;
    let content_type = answer
        .content_type
        .and_then(|value| HeaderValue::from_str(&value).ok())
        .unwrap_or(HeaderValue::from_static("text/html; charset=UTF-8"));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// `POST /D{id}/action`. Inline comments written on the page are sent to
/// PHP's `differential.revision.edit` with the rest, as PHP's web
/// interface has no form for them. The other changes are a post of PHP's
/// comment form, which Rust writes when it can, and PHP otherwise.
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
    let inlines = inline_transactions(&form.inlines).map_err(PageError::BadRequest)?;
    let has_inlines = !inlines.is_empty();
    let transactions = transactions(&form, &reviewers, &subscribers, inlines)
        .ok_or(PageError::BadRequest("This change can't be made."))?;
    if !has_inlines {
        let lists = (reviewers.as_slice(), subscribers.as_slice());
        let Some(comment) = comment_form(state.as_ref(), &form, lists, id).await? else {
            return Ok(see_other(&format!("/D{id}")));
        };
        return match web_edit::comment(state.as_ref(), &viewer, id, comment.clone()).await {
            Ok(()) => Ok(see_other(&format!("/D{id}"))),
            Err(PageError::Fallback(reason)) => {
                tracing::info!(reason, "comment form left to PHP");
                post_to_php(state.as_ref(), &viewer, id, &comment).await
            }
            Err(error) => Err(error),
        };
    }
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
    // phab.js forgets the inline comments it kept once they are published.
    Ok(see_other(&format!("/D{id}#inlines-published")))
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
            inlines: String::new(),
            remove_reviewer: String::new(),
            remove_subscriber: String::new(),
        }
    }

    #[test]
    fn reviewers_and_subscribers_are_removed_by_phid() {
        let mut removing = form("", "");
        removing.remove_reviewer = "PHID-USER-abc".to_owned();
        removing.remove_subscriber = "PHID-PROJ-def".to_owned();
        assert_eq!(
            transactions(&removing, &[], &[], vec![]),
            Some(vec![
                json!({"type": "reviewers.remove", "value": ["PHID-USER-abc"]}),
                json!({"type": "subscribers.remove", "value": ["PHID-PROJ-def"]}),
            ])
        );
        for wrong in ["PHID-DREV-abc", "alice", "PHID-USER-a\"b"] {
            removing.remove_reviewer = wrong.to_owned();
            assert_eq!(transactions(&removing, &[], &[], vec![]), None, "{wrong}");
        }
    }

    #[test]
    fn inline_comments_become_inline_transactions() {
        let inlines = r#"[
            {"path": "src/a.c", "line": 2, "isNewFile": false, "diff": 7, "content": "Why?"},
            {"path": "src/a.c", "line": 4, "length": 3, "content": "Typo"},
            {"replyToCommentPHID": "PHID-XCMT-1", "content": "Fixed."}
        ]"#;
        assert_eq!(
            inline_transactions(inlines),
            Ok(vec![
                json!({"type": "inline", "value": {"path": "src/a.c", "line": 2, "length": 1,
                    "isNewFile": false, "diff": 7, "content": "Why?"}}),
                json!({"type": "inline", "value": {"path": "src/a.c", "line": 4, "length": 3,
                    "isNewFile": true, "content": "Typo"}}),
                json!({"type": "inline", "value": {"replyToCommentPHID": "PHID-XCMT-1",
                    "content": "Fixed."}}),
            ])
        );
        assert_eq!(inline_transactions(" "), Ok(vec![]));
        for wrong in [
            "{",
            r#"[{"path": "a", "line": 0, "content": "x"}]"#,
            r#"[{"path": "a", "line": 1, "content": "  "}]"#,
            r#"[{"path": "", "line": 1, "content": "x"}]"#,
            r#"[{"replyToCommentPHID": "PHID-USER-1", "content": "x"}]"#,
            r#"[{"line": 1, "content": "x"}]"#,
        ] {
            assert!(inline_transactions(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn actions_and_comments_become_transactions() {
        assert_eq!(
            transactions(&form("accept", "LGTM"), &[], &[], vec![]),
            Some(vec![
                json!({"type": "accept", "value": true}),
                json!({"type": "comment", "value": "LGTM"})
            ])
        );
        assert_eq!(
            transactions(&form("", "  "), &[], &[], vec![]),
            Some(vec![])
        );
        assert_eq!(transactions(&form("destroy", ""), &[], &[], vec![]), None);
        assert_eq!(
            transactions(
                &form("", "Over to you"),
                &[
                    (Phid::new("PHID-USER-a"), false),
                    (Phid::new("PHID-PROJ-b"), true)
                ],
                &[(Phid::new("PHID-USER-c"), false)],
                vec![],
            ),
            Some(vec![
                json!({"type": "reviewers.add", "value": ["PHID-USER-a", "blocking(PHID-PROJ-b)"]}),
                json!({"type": "subscribers.add", "value": ["PHID-USER-c"]}),
                json!({"type": "comment", "value": "Over to you"}),
            ])
        );
    }

    #[test]
    fn lists_are_edited_as_the_tokenizer_does() {
        let list = |items: &[&str]| -> Vec<String> {
            items.iter().map(|item| (*item).to_owned()).collect()
        };
        let current = list(&["PHID-USER-a", "blocking(PHID-USER-b)"]);
        assert_eq!(
            edited_list(&current, &list(&["PHID-USER-c", "PHID-USER-a"]), ""),
            list(&["PHID-USER-a", "blocking(PHID-USER-b)", "PHID-USER-c"])
        );
        // Making a reviewer blocking replaces their token.
        assert_eq!(
            edited_list(&current, &list(&["blocking(PHID-USER-a)"]), ""),
            list(&["blocking(PHID-USER-a)", "blocking(PHID-USER-b)"])
        );
        assert_eq!(
            edited_list(&current, &[], "PHID-USER-b"),
            list(&["PHID-USER-a"])
        );
        assert_eq!(list_action("reviewers", &current, &current), None);
        assert_eq!(
            list_action("subscriberPHIDs", &[], &list(&["PHID-USER-a"])),
            Some(FormAction {
                kind: "subscriberPHIDs".to_owned(),
                value: json!(["PHID-USER-a"]),
                initial: json!([]),
            })
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
