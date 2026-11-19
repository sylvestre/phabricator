//! `subscribers.add`, `subscribers.remove` and `subscribers.set`: the
//! `core:subscribers` transaction of the request.

use phab_conduit::{ApiRequest, MethodError};

use super::lists::{self, Wanted};
use super::{Facts, Subscription};
use crate::storage::Revision;

/// The subscriptions of users and projects. Subscribing or unsubscribing
/// others needs `CAN_EDIT` (PHP throws without it, revisions having no
/// `CAN_INTERACT`); oneself, nothing. Updates the subscribers of `facts`.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &Revision,
    facts: &mut Facts,
    actor: &str,
) -> Result<Subscription, MethodError> {
    let old = facts.subscribers.clone();
    let new = lists::new_list(&old, wanted);
    let (added, removed) = (lists::minus(&new, &old), lists::minus(&old, &new));
    if added.is_empty() && removed.is_empty() {
        return Err(MethodError::Unsupported("edits without effect"));
    }
    let added_refs: Vec<&str> = added.iter().map(String::as_str).collect();
    if !added.is_empty() && !lists::all_valid(request, &added_refs).await? {
        return Err(MethodError::Unsupported("subscribers PHP doesn't know"));
    }
    if added.iter().chain(&removed).any(|phid| phid != actor) {
        let context = request.context;
        let can_edit = crate::storage::can_edit(
            &context.database,
            &context.config,
            &request.viewer,
            revision,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        if !can_edit {
            return Err(MethodError::Unsupported("subscriptions PHP refuses"));
        }
    }
    // The new edges are the newest, the last numbered first.
    let mut after: Vec<String> = added.iter().rev().cloned().collect();
    after.extend(lists::minus(&old, &removed));
    facts.subscribers = after;
    Ok(Subscription {
        phid: super::transaction_phid(),
        old,
        new,
        first: false,
    })
}
