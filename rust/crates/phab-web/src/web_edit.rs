//! The writes of the revision page, made as PHP's web interface makes them
//! (W6): the comment form of `DifferentialRevisionEditEngine`
//! (`PhabricatorEditEngine::buildCommentResponse()`) and the "Subscribe"
//! and "Unsubscribe" actions (`PhabricatorSubscriptionsEditController`).
//! Both run the Rust editor ([`phab_app_differential::edit`]) with a web
//! content source, after turning the form into the Conduit transactions
//! PHP's fields generate for it.
//!
//! What the comment form adds to the editor: the comment's
//! `remarkup.control` metadata (the posted `comment_metadata`), and once
//! the edit is saved, the deletion of the viewer's versioned draft of the
//! comment (`PhabricatorVersionedDraft::purgeDrafts()`) and of the "has
//! draft" edge (`PhabricatorDraftEngine::synchronize()`).
//!
//! Left to PHP (`421`, before writing): workflow (Ajax) requests, previews
//! and "continue" requests, hisec posts, invalid CSRF tokens, drafts
//! (whose subscribers raise warnings), revisions with configured edit
//! forms, viewers with unpublished inline comments or "done" states to
//! publish (PHP publishes them with the comment), comment actions other
//! than the review and author actions and the reviewer, subscriber and tag
//! changes, reviewer or tag changes which both add and remove, files
//! attached by drag and drop, and every edit the editor leaves to PHP
//! (which includes those PHP refuses or finds without effect, which it
//! explains in a dialog).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::{Router, routing};
use phab_app_differential::edit::{self, Source};
use phab_app_differential::storage::{self, Revision};
use phab_conduit::{ApiRequest, Context, MethodError};
use phab_core::{Phid, PhidType};
use phab_policy::application;
use serde_json::{Map, Value, json};

use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `DifferentialRevisionEditEngine::ENGINECONST`.
const ENGINE_KEY: &str = "differential.revision";

/// `PhabricatorObjectHasDraftEdgeType`.
const OBJECT_HAS_DRAFT: i64 = 64;

/// `PhabricatorSubscriptionsApplication`.
const SUBSCRIPTIONS: &str = "PhabricatorSubscriptionsApplication";

/// The `ACTIONKEY`s of the revision actions the editor may apply, and
/// whether they are review actions (`ACTIONGROUP_REVIEW`), which don't need
/// `CAN_EDIT`.
const ACTIONS: &[(&str, bool)] = &[
    ("accept", true),
    ("reject", true),
    ("resign", true),
    ("request-review", false),
    ("plan-changes", false),
    ("abandon", false),
    ("reclaim", false),
];

/// The tokenizer fields of the comment form (`setUseEdgeTransactions()`):
/// their keys and the Conduit edit type of their transactions.
const LISTS: &[(&str, &str)] = &[
    ("reviewers", "reviewers"),
    ("subscriberPHIDs", "subscribers"),
    ("projectPHIDs", "projects"),
];

/// An entry of `editengine.actions`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FormAction {
    /// The field's key: an `ACTIONKEY`, `reviewers`, ...
    pub(crate) kind: String,
    pub(crate) value: Value,
    /// `initialValue`: the value the form started with, `null` when it
    /// had none (the field then sets its value).
    pub(crate) initial: Value,
}

/// A post of the comment form, as `buildCommentResponse()` reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CommentForm {
    /// The actions, one of each type, in the order PHP applies them.
    pub(crate) actions: Vec<FormAction>,
    /// `comment`.
    pub(crate) comment: Option<String>,
    /// `comment_metadata`, decoded.
    pub(crate) metadata: Value,
}

impl CommentForm {
    /// The form of `request`, or why it is left to PHP.
    fn from_request(request: &LoginRequest) -> Result<Self, &'static str> {
        let actions = match request.str("editengine.actions") {
            // `if ($actions)`.
            None | Some("" | "0") => Vec::new(),
            Some(text) => parse_actions(text)?,
        };
        let metadata = match request.str("comment_metadata") {
            None => Value::Null,
            Some("") => json!(""),
            Some(text) => parse_metadata(text)?,
        };
        Ok(Self {
            actions,
            comment: request.str("comment").map(str::to_owned),
            metadata,
        })
    }

    /// The fields PHP's form posts for this, to send the form to PHP.
    pub(crate) fn php_fields(&self, csrf_token: &str) -> Vec<(&'static str, String)> {
        let actions: Vec<Value> = self
            .actions
            .iter()
            .map(|action| {
                json!({"type": action.kind, "value": action.value, "initialValue": action.initial})
            })
            .collect();
        let mut fields = vec![
            ("__form__", "1".to_owned()),
            ("__csrf__", csrf_token.to_owned()),
            ("editengine.actions", Value::Array(actions).to_string()),
        ];
        if let Some(comment) = &self.comment {
            fields.push(("comment", comment.clone()));
        }
        match &self.metadata {
            Value::Null => {}
            Value::String(text) => fields.push(("comment_metadata", text.clone())),
            // What the form posts.
            Value::Array(list) if list.is_empty() => {
                fields.push(("comment_metadata", "{}".to_owned()));
            }
            metadata => fields.push(("comment_metadata", metadata.to_string())),
        }
        fields
    }
}

/// `editengine.actions`: a JSON list of actions, of which PHP keeps the
/// last of each type, in the place of the first.
fn parse_actions(text: &str) -> Result<Vec<FormAction>, &'static str> {
    let unusual = "unusual comment actions";
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(text) else {
        return Err(unusual);
    };
    let mut actions: Vec<FormAction> = Vec::new();
    for entry in entries {
        let Value::Object(mut entry) = entry else {
            return Err(unusual);
        };
        // `if (!$type) continue;`
        let kind = match entry.get("type") {
            None | Some(Value::Null) => continue,
            Some(Value::String(kind)) if kind.is_empty() || kind == "0" => continue,
            Some(Value::String(kind)) => kind.clone(),
            Some(_) => return Err(unusual),
        };
        // The form always sends `initialValue`; without it, the field's
        // own initial value applies.
        let initial = entry
            .remove("initialValue")
            .ok_or("comment actions without an initial value")?;
        let action = FormAction {
            kind,
            value: entry.remove("value").unwrap_or(Value::Null),
            initial,
        };
        match actions.iter_mut().find(|known| known.kind == action.kind) {
            Some(known) => *known = action,
            None => actions.push(action),
        }
    }
    Ok(actions)
}

/// `comment_metadata`: the form's is `{}`, and lists the files dropped on
/// the text area, which PHP attaches.
fn parse_metadata(text: &str) -> Result<Value, &'static str> {
    match serde_json::from_str::<Value>(text) {
        // phutil_json_decode() of `{}` is an empty PHP array.
        Ok(Value::Object(map)) if map.is_empty() => Ok(json!([])),
        Ok(Value::Array(list)) if list.is_empty() => Ok(json!([])),
        Ok(Value::Object(map))
            if map.len() == 1 && map.get("attachedFilePHIDs") == Some(&json!([])) =>
        {
            Ok(Value::Object(map))
        }
        _ => Err("comments with attached files or unusual metadata"),
    }
}

/// The strings of a tokenizer value.
fn strings(value: &Value) -> Option<Vec<&str>> {
    value.as_array()?.iter().map(Value::as_str).collect()
}

/// `array_fuse()` of a list: its distinct items, in order.
fn fuse<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut fused: Vec<&str> = Vec::new();
    for item in items {
        if !fused.contains(&item) {
            fused.push(item);
        }
    }
    fused
}

/// The transaction `getValueForTransaction()` of an edge field makes from
/// the comment action, as a Conduit one: `+` is `.add`, `-` is `.remove`,
/// `=` (no initial value) is `.set`.
fn list_transaction(edit_type: &str, action: &FormAction) -> Result<Value, &'static str> {
    let unusual = "unusual tokenizer values";
    let new = strings(&action.value).ok_or(unusual)?;
    if action.initial.is_null() {
        return Ok(json!({"type": format!("{edit_type}.set"), "value": fuse(new)}));
    }
    let old = strings(&action.initial).ok_or(unusual)?;
    let added = fuse(new.iter().copied().filter(|item| !old.contains(item)));
    let removed = fuse(old.iter().copied().filter(|item| !new.contains(item)));
    match (added.is_empty(), removed.is_empty()) {
        (false, true) => Ok(json!({"type": format!("{edit_type}.add"), "value": added})),
        (true, false) => Ok(json!({"type": format!("{edit_type}.remove"), "value": removed})),
        // Both are one transaction, which Conduit can't say; neither is
        // an edit without effect.
        (false, false) => Err("list changes which add and remove"),
        (true, true) => Err("list changes without effect"),
    }
}

/// The Conduit transactions of the form: its actions' (`generateTransactions()`),
/// then the comment, if it has text or there is nothing else. Also says
/// whether an action needs `CAN_EDIT`.
fn transactions(form: &CommentForm) -> Result<(Vec<Value>, bool), &'static str> {
    let mut transactions = Vec::new();
    let mut needs_edit = false;
    for action in &form.actions {
        if let Some((_, is_review)) = ACTIONS.iter().find(|(key, _)| *key == action.kind) {
            if action.value != Value::Bool(true) {
                return Err("actions with options");
            }
            needs_edit |= !is_review;
            transactions.push(json!({"type": action.kind, "value": true}));
        } else if let Some((_, edit_type)) = LISTS.iter().find(|(key, _)| *key == action.kind) {
            needs_edit = true;
            transactions.push(list_transaction(edit_type, action)?);
        } else {
            return Err("comment actions not ported");
        }
    }
    let comment = form.comment.as_deref().unwrap_or_default();
    if !comment.is_empty() || transactions.is_empty() {
        transactions.push(json!({"type": "comment", "value": comment}));
    }
    Ok((transactions, needs_edit))
}

/// A Conduit request of the viewer, for the editor.
fn request<'a, S: WebState>(
    state: &S,
    context: &'a Context,
    viewer: &WebUser,
    params: Map<String, Value>,
) -> ApiRequest<'a> {
    ApiRequest {
        context,
        user: Some(viewer.user.clone()),
        viewer: viewer.user.viewer(),
        params,
        strictly_typed: true,
        now: state.now(),
        writes: phab_auth::Writes::Allowed,
    }
}

/// The editor's context, with the service's host name for Herald's
/// transcripts.
fn context<S: WebState>(state: &S) -> Context {
    Context {
        database: state.database().clone(),
        config: state.config().clone(),
        hostname: state.hostname().unwrap_or_default().to_owned(),
    }
}

/// Runs the editor on revision `id` with `transactions`; what it leaves to
/// PHP is a fallback, as nothing is written yet.
async fn apply<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
    transactions: Vec<Value>,
    source: Source,
) -> Result<(), PageError> {
    let context = context(state);
    let mut params = Map::new();
    params.insert("objectIdentifier".to_owned(), json!(id));
    params.insert("transactions".to_owned(), Value::Array(transactions));
    let request = request(state, &context, viewer, params);
    let files: edit::FileFinder = phab_app_files::search::embedded_files;
    match edit::apply(&request, Some(files), source).await {
        Ok(_) => Ok(()),
        Err(MethodError::Unsupported(reason)) => Err(PageError::Fallback(reason)),
        // PHP explains its refusals in a dialog.
        Err(MethodError::Conduit { .. }) => Err(PageError::Fallback("edits PHP refuses")),
        Err(MethodError::Internal(error)) => {
            Err(PageError::Internal(Box::new(std::io::Error::other(error))))
        }
    }
}

/// The revision `id`, if the viewer sees it; PHP answers the others.
async fn load_revision<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: Option<Revision>,
) -> Result<Revision, PageError> {
    let revision = revision.ok_or(PageError::Fallback("revisions which don't exist"))?;
    storage::visible_revisions(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        vec![revision],
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?
    .pop()
    .ok_or(PageError::Fallback("revisions the viewer can't see"))
}

/// Whether the viewer has inline comments `newAutomaticCommentTransactions()`
/// would publish: unpublished ones, or "done" states to publish on the
/// inlines of their own revision (`newInlineStateTransaction()`).
async fn has_inline_drafts<S: WebState>(
    state: &S,
    revision: &Revision,
    actor: &str,
) -> Result<bool, PageError> {
    let database = state.database();
    let is_author = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor);
    let sql = format!(
        "SELECT 1 FROM {} WHERE revisionPHID = ? AND ((authorPHID = ? AND isDeleted = 0 AND \
         transactionPHID IS NULL) OR (? AND transactionPHID IS NOT NULL AND fixedState IN \
         ('draft', 'undraft'))) LIMIT 1",
        database.table("differential", "differential_transaction_comment")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(actor)
        .bind(is_author)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// Whether revision forms are configured: their locks, defaults and
/// policies would apply (`loadDefaultEditConfiguration()`).
async fn has_configured_forms<S: WebState>(state: &S) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE engineKey = ? LIMIT 1",
        database.table("search", "search_editengineconfiguration")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(ENGINE_KEY)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// What `buildCommentResponse()` does once the edit is saved:
/// `purgeDrafts()`, then `synchronize()` of the draft engine, which finds
/// no draft left and removes the "has draft" edge.
async fn purge_drafts<S: WebState>(
    state: &S,
    revision: &Revision,
    actor: &str,
) -> Result<(), PageError> {
    let database = state.database();
    let drafts = format!(
        "DELETE FROM {} WHERE objectPHID = ? AND authorPHID = ?",
        database.table("draft", "draft_versioneddraft")
    );
    phab_db::sqlx::query(&drafts)
        .bind(revision.phid.as_str())
        .bind(actor)
        .execute(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    // The edge type has no inverse.
    let edge = format!(
        "DELETE FROM {} WHERE src = ? AND type = ? AND dst = ?",
        database.table("differential", "edge")
    );
    phab_db::sqlx::query(&edge)
        .bind(revision.phid.as_str())
        .bind(OBJECT_HAS_DRAFT)
        .bind(actor)
        .execute(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    Ok(())
}

/// Posts `form` on revision `id` as the viewer, as PHP's comment form does,
/// when Rust writes what PHP would; [`PageError::Fallback`] otherwise,
/// before anything is written.
pub(crate) async fn comment<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
    form: CommentForm,
) -> Result<(), PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let found = storage::load_revisions_by_id(state.database(), &[id])
        .await?
        .pop();
    let revision = load_revision(state, viewer, found).await?;
    if revision.status == "draft" {
        return Err(PageError::Fallback("comments on drafts"));
    }
    if has_configured_forms(state).await? {
        return Err(PageError::Fallback("configured revision forms"));
    }
    let actor = viewer.user.phid.as_str();
    if has_inline_drafts(state, &revision, actor).await? {
        return Err(PageError::Fallback("inline comments to publish"));
    }
    let (transactions, needs_edit) = transactions(&form).map_err(PageError::Fallback)?;
    if needs_edit {
        let can_edit = storage::can_edit(
            state.database(),
            state.config(),
            &viewer.user.viewer(),
            &revision,
            state.now(),
        )
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;
        if !can_edit {
            // PHP raises a policy exception.
            return Err(PageError::Fallback("actions the viewer may not take"));
        }
    }
    let source = Source::Web {
        remarkup_control: form.metadata,
    };
    apply(state, viewer, id, transactions, source).await?;
    purge_drafts(state, &revision, actor).await
}

/// Whether the request is a plain form post PHP's controllers accept
/// (`isFormOrHisecPost()`), with a valid CSRF token; PHP answers the
/// others, and the workflow requests its pages make.
fn check_form_post<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
) -> Result<(), PageError> {
    if !request.is_post() || !request.exists("__form__") || request.exists("__hisec__") {
        return Err(PageError::Fallback("requests which aren't form posts"));
    }
    if request.exists("__ajax__") || request.exists("__wflow__") {
        return Err(PageError::Fallback("a workflow request"));
    }
    let token = request.str("__csrf__").unwrap_or_default();
    if !viewer.is_valid_csrf_token(token, state.now())? {
        return Err(PageError::Fallback("invalid CSRF tokens"));
    }
    Ok(())
}

/// The redirect PHP answers a saved form with (`AphrontRedirectResponse`
/// to the revision).
fn back<S: WebState>(state: &S, request: LoginRequest, id: i64) -> Response {
    request.redirect(state.config(), &format!("/D{id}"))
}

/// `POST /differential/revision/edit/{id}/comment/`: the comment form.
pub(crate) async fn comment_form<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    viewer
        .require_application(&*state, application::DIFFERENTIAL)
        .await?;
    let id = crate::revision::parse_monogram(&format!("D{id}"))
        .ok_or(PageError::Fallback("unusual revision IDs"))?;
    let request = LoginRequest::new(method, uri, headers, &body);
    check_form_post(state.as_ref(), &viewer, &request)?;
    for name in ["__preview__", "__continue__"] {
        if request.exists(name) {
            return Err(PageError::Fallback("previews and confirmations"));
        }
    }
    let form = CommentForm::from_request(&request).map_err(PageError::Fallback)?;
    comment(state.as_ref(), &viewer, id, form).await?;
    Ok(back(state.as_ref(), request, id))
}

/// `POST /subscriptions/{add|delete}/{phid}/`: the viewer subscribes to a
/// revision or unsubscribes. Other objects and the authors and reviewers
/// of the revision, who are subscribed automatically (PHP says so in a
/// dialog), are left to PHP.
pub(crate) async fn subscription<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((action, phid)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let is_add = match action.as_str() {
        "add" => true,
        "delete" => false,
        _ => return Err(PageError::Fallback("unusual subscription actions")),
    };
    viewer.require_application(&*state, SUBSCRIPTIONS).await?;
    let request = LoginRequest::new(method, uri, headers, &body);
    check_form_post(state.as_ref(), &viewer, &request)?;
    let phid = Phid::new(phid.as_str());
    if phid.phid_type() != PhidType::REVISION {
        return Err(PageError::Fallback("subscriptions to other objects"));
    }
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let found = storage::load_revisions(state.database(), std::slice::from_ref(&phid))
        .await?
        .pop();
    let revision = load_revision(state.as_ref(), &viewer, found).await?;
    let actor = viewer.user.phid.as_str();
    // isAutomaticallySubscribed().
    let is_automatic = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor)
        || revision
            .reviewers
            .iter()
            .any(|reviewer| reviewer.phid.as_str() == actor && reviewer.status != "resigned");
    if is_automatic {
        return Err(PageError::Fallback("automatic subscriptions"));
    }
    let kind = if is_add {
        "subscribers.add"
    } else {
        "subscribers.remove"
    };
    let transactions = vec![json!({"type": kind, "value": [actor]})];
    let source = Source::Web {
        remarkup_control: Value::Null,
    };
    apply(state.as_ref(), &viewer, revision.id, transactions, source).await?;
    // AphrontReloadResponse, to the handle's URI.
    Ok(back(state.as_ref(), request, revision.id))
}

/// PHP's routes of the comment form and of the subscribe action.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/differential/revision/edit/{id}/comment/",
            routing::post(comment_form::<S>),
        )
        .route(
            "/subscriptions/{action}/{phid}/",
            routing::post(subscription::<S>),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: &str, value: Value, initial: Value) -> FormAction {
        FormAction {
            kind: kind.to_owned(),
            value,
            initial,
        }
    }

    fn form(actions: Vec<FormAction>, comment: &str) -> CommentForm {
        CommentForm {
            actions,
            comment: Some(comment.to_owned()),
            metadata: json!([]),
        }
    }

    #[test]
    fn actions_keep_the_last_of_each_type_in_the_first_place() {
        let actions = parse_actions(
            r#"[{"type": "accept", "value": true, "initialValue": null},
                {"type": "", "value": 1},
                {"value": 2},
                {"type": "reviewers", "value": ["PHID-USER-a"], "initialValue": []},
                {"type": "accept", "value": false, "initialValue": null}]"#,
        )
        .expect("actions");
        assert_eq!(
            actions,
            vec![
                action("accept", json!(false), Value::Null),
                action("reviewers", json!(["PHID-USER-a"]), json!([])),
            ]
        );
        assert!(parse_actions("{}").is_err());
        assert!(parse_actions("[1]").is_err());
        assert!(parse_actions(r#"[{"type": "accept", "value": true}]"#).is_err());
    }

    #[test]
    fn metadata_is_decoded_like_php() {
        assert_eq!(parse_metadata("{}"), Ok(json!([])));
        assert_eq!(
            parse_metadata(r#"{"attachedFilePHIDs": []}"#),
            Ok(json!({"attachedFilePHIDs": []}))
        );
        assert!(parse_metadata(r#"{"attachedFilePHIDs": ["PHID-FILE-1"]}"#).is_err());
        assert!(parse_metadata("nope").is_err());
    }

    #[test]
    fn lists_become_additions_removals_or_sets() {
        let reviewers = |value, initial| {
            transactions(&form(vec![action("reviewers", value, initial)], ""))
                .map(|(transactions, _)| transactions)
        };
        assert_eq!(
            reviewers(json!(["a", "blocking(b)", "c", "c"]), json!(["a"])),
            Ok(vec![
                json!({"type": "reviewers.add", "value": ["blocking(b)", "c"]})
            ])
        );
        assert_eq!(
            reviewers(json!(["a"]), json!(["a", "b"])),
            Ok(vec![json!({"type": "reviewers.remove", "value": ["b"]})])
        );
        assert_eq!(
            reviewers(json!(["b", "b"]), Value::Null),
            Ok(vec![json!({"type": "reviewers.set", "value": ["b"]})])
        );
        assert!(reviewers(json!(["blocking(a)"]), json!(["a"])).is_err());
        assert!(reviewers(json!(["a"]), json!(["a"])).is_err());
        assert!(reviewers(json!("a"), json!([])).is_err());
    }

    #[test]
    fn the_comment_comes_last_or_alone() {
        let subscribe = action("subscriberPHIDs", json!(["PHID-USER-a"]), json!([]));
        assert_eq!(
            transactions(&form(
                vec![action("accept", json!(true), Value::Null), subscribe],
                "LGTM"
            )),
            Ok((
                vec![
                    json!({"type": "accept", "value": true}),
                    json!({"type": "subscribers.add", "value": ["PHID-USER-a"]}),
                    json!({"type": "comment", "value": "LGTM"}),
                ],
                true
            ))
        );
        assert_eq!(
            transactions(&form(vec![action("reject", json!(true), Value::Null)], "")),
            Ok((vec![json!({"type": "reject", "value": true})], false))
        );
        assert_eq!(
            transactions(&form(vec![], "")),
            Ok((vec![json!({"type": "comment", "value": ""})], false))
        );
        let commandeer = action("commandeer", json!(true), Value::Null);
        assert!(transactions(&form(vec![commandeer], "")).is_err());
        let accept_for = action("accept", json!(["PHID-PROJ-a"]), Value::Null);
        assert!(transactions(&form(vec![accept_for], "")).is_err());
    }

    #[test]
    fn forms_go_to_php_as_php_posts_them() {
        let form = form(vec![action("accept", json!(true), Value::Null)], "Ok");
        assert_eq!(
            form.php_fields("B@token"),
            vec![
                ("__form__", "1".to_owned()),
                ("__csrf__", "B@token".to_owned()),
                (
                    "editengine.actions",
                    r#"[{"type":"accept","value":true,"initialValue":null}]"#.to_owned()
                ),
                ("comment", "Ok".to_owned()),
                ("comment_metadata", "{}".to_owned()),
            ]
        );
    }
}
