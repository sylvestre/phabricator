//! `/herald/webhook/`: the builtin queries of `HeraldWebhookSearchEngine`
//! (Active, All). Filters and saved queries are PHP's, and so are the
//! webhook pages and their forms.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_harbormaster::web as harbormaster_web;
use phab_app_herald::web::{self as herald, WebhookRow};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, PolicySubject};

use super::APPLICATION;
use super::lists::cursor;
use crate::error::PageError;
use crate::harbormaster::internal;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

/// A webhook in the list.
pub(crate) struct WebhookItem {
    id: i64,
    name: String,
    uri: String,
    icon: &'static str,
    status: &'static str,
    disabled: bool,
}

#[derive(Template)]
#[template(path = "herald_webhooks.html")]
pub(crate) struct WebhookList {
    shell: Shell,
    /// `HeraldCreateWebhooksCapability`: PHP disables the action otherwise.
    can_create: bool,
    query: &'static str,
    title: &'static str,
    items: Vec<WebhookItem>,
    next: Option<i64>,
}

/// `getStatusSpecifications()`: the icon and name of a status.
fn status(status: &str) -> (&'static str, &'static str) {
    match status {
        "firehose" => ("fa-star-o", "Firehose"),
        "enabled" => ("fa-check", "Enabled"),
        "disabled" => ("fa-ban", "Disabled"),
        _ => ("fa-question-circle", "Unknown"),
    }
}

/// The webhooks of `rows` the viewer can see.
async fn visible<S: WebState>(
    state: &S,
    viewer: &WebUser,
    rows: Vec<WebhookRow>,
) -> Result<Vec<WebhookRow>, PageError> {
    let subjects: Vec<PolicySubject> = rows
        .iter()
        .map(|row| {
            PolicySubject::new(Some(row.phid.clone()), ObjectKind::Other)
                .with_policy(Capability::VIEW, row.view_policy.clone())
        })
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let as_viewer = viewer.user.viewer();
    let facts = phab_policy::loader::load_facts(
        state.database(),
        state.config(),
        &as_viewer,
        &refs,
        &NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let passed =
        phab_policy::filter(&as_viewer, &refs, &[Capability::VIEW], &facts).map_err(internal)?;
    Ok(rows
        .into_iter()
        .zip(passed)
        .filter_map(|(row, visible)| visible.then_some(row))
        .collect())
}

async fn list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<WebhookList>, PageError> {
    let after = cursor(query)?;
    let statuses: Option<&[&str]> = (key == "active").then_some(&["firehose", "enabled"]);
    // `PhabricatorCursorPagedPolicyAwareQuery`: pages of rows until enough
    // of them are visible.
    let mut shown = Vec::new();
    let mut before = after;
    loop {
        let rows = herald::webhooks(state.database(), statuses, before, PAGE_SIZE + 1).await?;
        let exhausted = rows.len() <= PAGE_SIZE;
        before = rows.last().map(|row| row.id);
        shown.extend(visible(state, viewer, rows).await?);
        if shown.len() > PAGE_SIZE || exhausted {
            break;
        }
    }
    let next = (shown.len() > PAGE_SIZE).then(|| shown[PAGE_SIZE - 1].id);
    shown.truncate(PAGE_SIZE);
    let items = shown
        .into_iter()
        .map(|row| {
            let (icon, name) = status(&row.status);
            WebhookItem {
                id: row.id,
                name: row.name,
                uri: row.webhook_uri,
                icon,
                status: name,
                disabled: row.status == "disabled",
            }
        })
        .collect();
    let can_create = crate::favorites::has_application_capability(
        state,
        &viewer.user.viewer(),
        APPLICATION,
        ("herald.webhooks", "admin"),
    )
    .await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(WebhookList {
        shell: Shell::for_user(state.config(), viewer, "Webhooks", current),
        can_create,
        query: key,
        title: if key == "active" { "Active" } else { "All" },
        items,
        next,
    }))
}

/// `GET /herald/webhook/`: the viewer's default query, the first builtin
/// one ("Active") unless they saved or reordered queries.
pub(crate) async fn webhooks<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<WebhookList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if harbormaster_web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        "HeraldWebhookSearchEngine",
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    list(state.as_ref(), &viewer, "active", query.as_deref(), &uri).await
}

/// `GET /herald/webhook/query/{key}/`: a builtin query.
pub(crate) async fn webhooks_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<WebhookList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "active" => "active",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved search query")),
    };
    list(state.as_ref(), &viewer, key, query.as_deref(), &uri).await
}
