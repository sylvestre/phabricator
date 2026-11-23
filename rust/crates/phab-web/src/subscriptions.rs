//! `/subscriptions/list/<phid>/` (`PhabricatorSubscriptionsListController`):
//! the subscribers of a revision, as a page for direct links. PHP's pages
//! open it as a dialog (an Ajax request), which stays with PHP, as do the
//! subscribers of other objects.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_differential::storage;
use phab_core::Phid;
use phab_db::edges;

use crate::error::PageError;
use crate::harbormaster::{Scope, handle, load_handles};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

#[derive(Template)]
#[template(path = "subscribers.html")]
pub(crate) struct SubscriberList {
    shell: Shell,
    object_uri: String,
    object_name: String,
    /// Each subscriber's link.
    subscribers: Vec<String>,
}

/// `GET /subscriptions/list/<phid>/`.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<SubscriberList>, PageError> {
    if form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .any(|(key, _)| key == "__ajax__" || key == "__dialog__")
    {
        return Err(PageError::Fallback("the subscribers dialog"));
    }
    if !phid.starts_with("PHID-DREV-") {
        return Err(PageError::Fallback("subscribers of an object not ported"));
    }
    viewer
        .require_application(&*state, "PhabricatorDifferentialApplication")
        .await?;
    let database = state.database();
    let phid = Phid::new(phid);
    let revision = storage::visible_revisions(
        database,
        state.config(),
        &viewer.user.viewer(),
        storage::load_revisions(database, std::slice::from_ref(&phid)).await?,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?
    .pop()
    .ok_or(PageError::NotFound)?;
    // `loadSubscribersForPHID()`: the newest first.
    let subscribers: Vec<Phid> = edges::load_edges(
        database,
        std::slice::from_ref(&phid),
        &[edges::OBJECT_HAS_SUBSCRIBER],
        None,
    )
    .await?
    .into_iter()
    .map(|edge| edge.dst)
    .collect();
    let scope = Scope::new(state.as_ref(), &viewer);
    let handles = load_handles(state.as_ref(), &scope, &subscribers).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(SubscriberList {
        shell: Shell::for_user(state.config(), &viewer, "Subscribers", current),
        object_uri: format!("/D{}", revision.id),
        object_name: format!("D{}: {}", revision.id, revision.title),
        subscribers: subscribers
            .iter()
            .map(|subscriber| handle(&handles, subscriber).link_html())
            .collect(),
    }))
}
