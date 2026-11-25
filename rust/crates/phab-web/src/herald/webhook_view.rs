//! A webhook (`/herald/webhook/view/<id>/[request/<requestID>/]`,
//! `HeraldWebhookViewController`), and its small forms: a test request
//! (`/herald/webhook/test/<id>/`, `HeraldWebhookTestController`) and its
//! HMAC key (`/herald/webhook/key/{view,cycle}/<id>/`,
//! `HeraldWebhookKeyController`). The webhook's timeline links to PHP;
//! the edit form is PHP's.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use phab_app_herald::web::{self as herald, RuleFilter};
use phab_app_herald::webhook::{self, Webhook};
use phab_core::Phid;
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, PolicySubject};

use super::APPLICATION;
use super::rule::visible_rules;
use crate::error::PageError;
use crate::harbormaster::{self, Action, Scope, internal};
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{project_edit, web_edit};

/// A row of "Recent Requests" (`HeraldWebhookRequestListView`).
pub(crate) struct RequestRow {
    id: i64,
    highlighted: bool,
    /// The status icon, its color and tooltip (`newStatusIcon()`).
    icon: (&'static str, &'static str, &'static str),
    object_html: String,
    error_type: String,
    error_code: String,
    requested_at: Option<String>,
}

#[derive(Template)]
#[template(path = "herald_webhook.html")]
pub(crate) struct WebhookPage {
    shell: Shell,
    id: i64,
    name: String,
    /// Icon, color and label of the header's status.
    status: (&'static str, &'static str, &'static str),
    uri: String,
    backoff: bool,
    silent: bool,
    actions: Vec<Action>,
    /// Links to the rules calling the webhook.
    rules_html: Vec<String>,
    phid: String,
    requests: Vec<RequestRow>,
}

/// `getStatusSpecifications()`: icon, color, name.
fn status_spec(status: &str) -> (&'static str, &'static str, &'static str) {
    match status {
        "firehose" => ("fa-star-o", "orange", "Firehose"),
        "enabled" => ("fa-check", "bluegrey", "Enabled"),
        "disabled" => ("fa-ban", "grey", "Disabled"),
        _ => ("fa-question-circle", "red", "Unknown"),
    }
}

/// Whether the viewer has `capability` on the webhook, whose policies
/// are its columns.
async fn has_capability<S: WebState>(
    state: &S,
    viewer: &WebUser,
    hook: &Webhook,
    capability: Capability,
) -> Result<bool, PageError> {
    let subject = PolicySubject::new(Some(hook.phid.clone()), ObjectKind::Other)
        .with_policy(Capability::VIEW, hook.view_policy.clone())
        .with_policy(Capability::EDIT, hook.edit_policy.clone());
    let as_viewer = viewer.user.viewer();
    let facts = phab_policy::loader::load_facts(
        state.database(),
        state.config(),
        &as_viewer,
        &[&subject],
        &NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    phab_policy::has_capability(&as_viewer, &subject, capability, &facts).map_err(internal)
}

/// `(?P<id>[1-9]\d*)`.
fn parse_id(id: &str) -> Result<i64, PageError> {
    Some(id)
        .filter(|id| {
            id.starts_with(|first: char| ('1'..='9').contains(&first))
                && id.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|id| id.parse().ok())
        .ok_or(PageError::NotFound)
}

/// The webhook the viewer sees; with `edit`, also edits
/// (`requireCapabilities()` raises the policy exception).
async fn load_hook<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: &str,
    edit: bool,
) -> Result<Webhook, PageError> {
    let id = parse_id(id)?;
    viewer.require_application(state, APPLICATION).await?;
    let hook = webhook::load(state.database(), id)
        .await?
        .ok_or(PageError::NotFound)?;
    // `executeOne()` raises the policy exception for hidden webhooks.
    if !has_capability(state, viewer, &hook, Capability::VIEW).await? {
        return Err(PageError::PolicyDenied);
    }
    if edit && !has_capability(state, viewer, &hook, Capability::EDIT).await? {
        return Err(PageError::PolicyDenied);
    }
    Ok(hook)
}

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// `buildCurtain()`: the forms, which need `CAN_EDIT`.
fn curtain(id: i64, can_edit: bool) -> Vec<Action> {
    [
        (
            "Edit Webhook",
            "fa-pencil",
            format!("/herald/webhook/edit/{id}/"),
        ),
        (
            "New Test Request",
            "fa-cloud-upload",
            format!("/herald/webhook/test/{id}/"),
        ),
        (
            "View HMAC Key",
            "fa-key",
            format!("/herald/webhook/key/view/{id}/"),
        ),
        (
            "Regenerate HMAC Key",
            "fa-refresh",
            format!("/herald/webhook/key/cycle/{id}/"),
        ),
    ]
    .into_iter()
    .map(|(name, icon, uri)| Action {
        name,
        icon,
        uri,
        enabled: can_edit,
    })
    .collect()
}

/// `GET /herald/webhook/view/{id}/`.
pub(crate) async fn view<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<WebhookPage>, PageError> {
    show(state.as_ref(), viewer, &id, None, &uri).await
}

/// `GET /herald/webhook/view/{id}/request/{requestID}/`: the same, with
/// the request highlighted.
pub(crate) async fn view_request<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((id, request_id)): Path<(String, String)>,
    uri: Uri,
) -> Result<Page<WebhookPage>, PageError> {
    let request_id = parse_id(&request_id)?;
    show(state.as_ref(), viewer, &id, Some(request_id), &uri).await
}

async fn show<S: WebState>(
    state: &S,
    viewer: WebUser,
    id: &str,
    highlight: Option<i64>,
    uri: &Uri,
) -> Result<Page<WebhookPage>, PageError> {
    let hook = load_hook(state, &viewer, id, false).await?;
    let database = state.database();
    let can_edit = has_capability(state, &viewer, &hook, Capability::EDIT).await?;
    let backoff = webhook::is_in_error_backoff(database, &hook.phid, state.now()).await?;
    let scope = Scope::new(state, &viewer);

    let rule_ids = webhook::calling_rules(database, &hook.phid, 10).await?;
    let rules = herald::load_rules(
        database,
        &RuleFilter {
            ids: Some(&rule_ids),
            ..RuleFilter::default()
        },
    )
    .await?;
    if rules.iter().any(herald::RuleRow::is_object_rule) {
        return Err(PageError::Fallback("webhooks called by object rules"));
    }
    let rules_html = visible_rules(state, &scope, rules)
        .await?
        .iter()
        .map(|rule| {
            harbormaster::link_html(
                &format!("{} {}", rule.monogram(), rule.name),
                Some(&format!("/{}", rule.monogram())),
                false,
            )
        })
        .collect();

    let requests = webhook::requests(database, &hook.phid, 20).await?;
    let objects: Vec<Phid> = requests
        .iter()
        .map(|request| request.object_phid.clone())
        .collect();
    let handles = super::load_handles(state, &scope, &objects).await?;
    let requests = requests
        .into_iter()
        .map(|request| {
            let error_type = request.error_type.clone().unwrap_or_default();
            let error_code = request.error_code.clone().unwrap_or_default();
            let error_type_name = match error_type.as_str() {
                "hook" => "Hook Error".to_owned(),
                "http" => "HTTP Status Code".to_owned(),
                "timeout" => "Request Timeout".to_owned(),
                _ => error_type.clone(),
            };
            let error_code_name = if error_type == "hook" {
                match error_code.as_str() {
                    "silent" => "In Silent Mode".to_owned(),
                    "disabled" => "Hook Disabled".to_owned(),
                    "uri" => "Invalid URI".to_owned(),
                    "object" => "Invalid Object".to_owned(),
                    _ => error_code,
                }
            } else {
                error_code
            };
            RequestRow {
                id: request.id,
                highlighted: highlight == Some(request.id),
                icon: match request.status.as_str() {
                    "queued" => ("fa-refresh", "blue", "Queued"),
                    "sent" => ("fa-check", "green", "Sent"),
                    _ => ("fa-times", "red", "Failed"),
                },
                object_html: harbormaster::handle(&handles, &request.object_phid).link_html(),
                error_type: error_type_name,
                error_code: error_code_name,
                requested_at: (request.last_request_epoch != 0)
                    .then(|| crate::time::format_utc(request.last_request_epoch)),
            }
        })
        .collect();

    let actions = curtain(hook.id, can_edit);
    let silent = state
        .config()
        .value("phabricator.silent")
        .is_some_and(phab_core::php::is_truthy);
    let title = format!("Webhook {}", hook.id);
    Ok(Page(WebhookPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current(uri)),
        id: hook.id,
        status: status_spec(&hook.status),
        uri: hook.webhook_uri,
        backoff,
        silent,
        actions,
        rules_html,
        phid: hook.phid.to_string(),
        requests,
        name: hook.name,
    }))
}

/// `GET|POST /herald/webhook/test/{id}/`: the dialog asking for an
/// object, and the post queueing the request. Objects other than
/// revisions (`D123`) are left to PHP.
pub(crate) async fn test<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    let hook = load_hook(state, &viewer, &id, true).await?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let view = format!("/herald/webhook/view/{}/", hook.id);
    if !request.is_post() {
        let paragraphs = vec![
            "Optionally, choose an object to generate test data for (like D123 or T234). The 10 \
             most recent transactions for the object will be submitted to the webhook."
                .to_owned(),
        ];
        return Ok(project_edit::dialog_page(
            state,
            &viewer,
            &uri,
            ("New Test Request", paragraphs, Some("Test Webhook")),
            vec![("object", String::new())],
            view,
        )
        .into_response());
    }
    web_edit::check_form_post(state, &viewer, &request)?;
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let name = request.str("object").unwrap_or_default();
    let database = state.database();
    let (object, transactions) = if name.is_empty() {
        let transactions = webhook::transaction_phids(database, &hook.phid, 10).await?;
        (hook.phid.to_string(), transactions)
    } else {
        return Err(PageError::Fallback("test requests about other objects"));
    };
    let request_id = webhook::queue_test_request(
        database,
        &hook.phid,
        (&object, viewer.user.phid.as_str(), &transactions),
        state.now(),
    )
    .await?;
    Ok(request.redirect(state.config(), &format!("{view}request/{request_id}/")))
}

#[derive(Template)]
#[template(path = "herald_webhook_key.html")]
pub(crate) struct KeyPage {
    shell: Shell,
    key: String,
    done: String,
}

/// `GET|POST /herald/webhook/key/{view|cycle}/{id}/`: the key, and for
/// `cycle` the confirmation whose post regenerates it first.
pub(crate) async fn key<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((action, id)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    if !matches!(action.as_str(), "view" | "cycle") {
        return Err(PageError::NotFound);
    }
    let state = state.as_ref();
    let mut hook = load_hook(state, &viewer, &id, true).await?;
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let view = format!("/herald/webhook/view/{}/", hook.id);
    if action == "cycle" {
        if !request.is_post() {
            return Ok(project_edit::dialog(
                state,
                &viewer,
                &uri,
                (
                    "Regenerate HMAC Key",
                    vec![
                        "Regenerate the HMAC key used to sign requests made by this webhook?",
                        "Requests which are currently authenticated with the old key may fail.",
                    ],
                    Some("Regnerate Key"),
                ),
                Vec::new(),
                view,
            ));
        }
        web_edit::check_form_post(state, &viewer, &request)?;
        if state.config().is_read_only() {
            return Err(PageError::Fallback("writes on a read-only install"));
        }
        hook.hmac_key = webhook::regenerate_key(state.database(), hook.id, state.now()).await?;
    }
    Ok(Page(KeyPage {
        shell: Shell::for_user(state.config(), &viewer, "Webhook HMAC Key", current(&uri)),
        key: hook.hmac_key,
        done: view,
    })
    .into_response())
}
