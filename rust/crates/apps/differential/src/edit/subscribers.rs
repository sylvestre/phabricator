//! `subscribers.add`, `subscribers.remove` and `subscribers.set`: the
//! `core:subscribers` transaction of the request.

use phab_conduit::{ApiRequest, MethodError};

use super::lists::{self, Operation, Wanted};
use super::{Facts, Subscription};
use crate::storage::Revision;

/// The subscriptions of users and projects, with the `implied` one of a
/// comment (its author, the users it mentions) merged in as
/// `combineTransactions()` does. Subscribing or unsubscribing others needs
/// `CAN_EDIT` (PHP throws without it, revisions having no `CAN_INTERACT`);
/// oneself, nothing. Updates the subscribers of `facts`.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    implied: Option<Subscription>,
    revision: &Revision,
    facts: &mut Facts,
    actor: &str,
) -> Result<Subscription, MethodError> {
    let (old, phid, first, plus) = match implied {
        Some(implied) => (implied.old, implied.phid, implied.first, implied.added),
        None => (
            facts.subscribers.clone(),
            super::transaction_phid(),
            false,
            Vec::new(),
        ),
    };
    let new = merged_list(&old, wanted, &plus, first);
    let (added, removed) = (lists::minus(&new, &old), lists::minus(&old, &new));
    if added.is_empty() && removed.is_empty() {
        return Err(MethodError::Unsupported("edits without effect"));
    }
    let explicit: Vec<&str> = if wanted.operation == Operation::Remove {
        Vec::new()
    } else {
        wanted.phids.iter().map(String::as_str).collect()
    };
    if !explicit.is_empty() && !lists::all_valid(request, &explicit).await? {
        return Err(MethodError::Unsupported("subscribers PHP doesn't know"));
    }
    if added.iter().chain(&removed).any(|phid| phid != actor) {
        let can_edit = super::can_edit(request, revision).await?;
        if !can_edit {
            return Err(MethodError::Unsupported("subscriptions PHP refuses"));
        }
    }
    // The new edges are the newest, the last numbered first.
    let mut after: Vec<String> = added.iter().rev().cloned().collect();
    after.extend(lists::minus(&old, &removed));
    facts.subscribers = after;
    Ok(Subscription {
        phid,
        old,
        new,
        first,
        added: plus,
    })
}

/// `getPHIDList()` of the merged transaction. The implicit CC comes first
/// and the mentions last, and `mergePHIDOrEdgeTransactions()` puts each
/// later `+` before the earlier: the mentions, the explicit additions,
/// then the actor. `plus` is the implied `+`: the mentions, then the actor
/// when `implicit`.
fn merged_list(old: &[String], wanted: &Wanted, plus: &[String], implicit: bool) -> Vec<String> {
    let (mentions, actor) = match plus.split_last() {
        Some((actor, mentions)) if implicit => (mentions, Some(actor)),
        _ => (plus, None),
    };
    let explicit_add: &[String] = if wanted.operation == Operation::Add {
        &wanted.phids
    } else {
        &[]
    };
    let set = (wanted.operation == Operation::Set).then_some(&wanted.phids);
    let mut result: Vec<String> = Vec::new();
    let mut put = |phid: &String| {
        if !result.contains(phid) {
            result.push(phid.clone());
        }
    };
    for phid in old {
        if set.is_none_or(|set| set.contains(phid)) {
            put(phid);
        }
    }
    set.into_iter().flatten().for_each(&mut put);
    mentions
        .iter()
        .chain(explicit_add)
        .chain(actor)
        .for_each(&mut put);
    if wanted.operation == Operation::Remove {
        result.retain(|phid| !wanted.phids.contains(phid));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn merges_like_php() {
        let old = strings(&["bob"]);
        let add = Wanted {
            operation: Operation::Add,
            phids: strings(&["dave"]),
        };
        assert_eq!(
            merged_list(&old, &add, &strings(&["carol", "alice"]), true),
            strings(&["bob", "carol", "dave", "alice"])
        );
        assert_eq!(
            merged_list(&old, &add, &strings(&["carol"]), false),
            strings(&["bob", "carol", "dave"])
        );
        let set = Wanted {
            operation: Operation::Set,
            phids: strings(&["dave"]),
        };
        assert_eq!(
            merged_list(&old, &set, &strings(&["alice"]), true),
            strings(&["dave", "alice"])
        );
        let remove = Wanted {
            operation: Operation::Remove,
            phids: strings(&["bob", "alice"]),
        };
        assert_eq!(
            merged_list(&old, &remove, &strings(&["alice"]), true),
            strings(&[] as &[&str])
        );
    }
}
