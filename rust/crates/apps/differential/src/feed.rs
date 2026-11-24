//! `shouldHideForFeed()` of Differential transactions, which decides whether
//! a feed story is shown (`PhabricatorApplicationTransactionFeedStory::
//! isVisibleInFeed()`).

use std::borrow::Cow;

use phab_db::edges;
use serde_json::Value;

use crate::storage::{Comment, Transaction};

const TYPE_SUBSCRIBERS: &str = "core:subscribers";
const TYPE_EDGE: &str = "core:edge";

/// Whether a transaction shows in feed. `Unknown` when the answer depends on
/// logic that is not ported (custom fields other than Mozilla's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedVisibility {
    /// Shown in feed.
    Visible,
    /// Hidden from feed.
    Hidden,
    /// Only PHP can tell.
    Unknown,
}

impl From<bool> for FeedVisibility {
    fn from(hidden: bool) -> Self {
        if hidden { Self::Hidden } else { Self::Visible }
    }
}

/// `isVisibleInFeed()`: a story shows if any of its transactions does.
pub fn story_visibility(transactions: &[FeedVisibility]) -> FeedVisibility {
    if transactions.contains(&FeedVisibility::Visible) {
        FeedVisibility::Visible
    } else if transactions.contains(&FeedVisibility::Unknown) {
        FeedVisibility::Unknown
    } else {
        FeedVisibility::Hidden
    }
}

fn truthy(value: Option<&Value>) -> bool {
    value.is_some_and(phab_core::php::is_truthy)
}

/// The integer value of an edge type, which PHP compares loosely.
fn edge_type(transaction: &Transaction) -> Option<i64> {
    match transaction.metadata_value("edge:type")? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// What PHP's `array_diff()` leaves of a list of strings.
struct Missing {
    count: usize,
    first: Option<String>,
}

/// `array_diff($from, $other)`; `None` if a value isn't a scalar.
fn missing(from: &[&Value], other: &[&Value]) -> Option<Missing> {
    let mut count = 0;
    let mut first = None;
    for value in from {
        let text = scalar_string(value)?;
        let present = other
            .iter()
            .any(|candidate| scalar_string(candidate).as_deref() == Some(&*text));
        if !present {
            count += 1;
            first.get_or_insert_with(|| text.into_owned());
        }
    }
    Some(Missing { count, first })
}

fn scalar_string(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(text) => Some(Cow::Borrowed(text)),
        Value::Number(number) => Some(Cow::Owned(number.to_string())),
        _ => None,
    }
}

fn as_list(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Object(entries) => Some(entries.values().collect()),
        _ => None,
    }
}

/// `isSelfSubscription()`; `None` if the values aren't lists (PHP throws).
pub fn is_self_subscription(transaction: &Transaction) -> Option<bool> {
    if transaction.transaction_type != TYPE_SUBSCRIBERS {
        return Some(false);
    }
    let old = as_list(&transaction.old_value)?;
    let new = as_list(&transaction.new_value)?;
    let added = missing(&old, &new)?;
    let removed = missing(&new, &old)?;
    if added.count + removed.count != 1 {
        return Some(false);
    }
    let affected = added.first.or(removed.first).unwrap_or_default();
    Some(affected == transaction.author_phid.as_str())
}

/// `PhabricatorApplicationTransaction::shouldHide()`, which every
/// application's transactions share; `comment` is the transaction's, if the
/// viewer can see it.
pub fn should_hide(transaction: &Transaction, comment: Option<&Comment>) -> FeedVisibility {
    if comment.is_some_and(|comment| !comment.is_empty()) {
        return FeedVisibility::Visible;
    }
    let kind = transaction.transaction_type.as_str();
    if kind == "core:history" {
        return FeedVisibility::Hidden;
    }
    if kind == "core:file" && truthy(transaction.metadata_value("attach.implicit")) {
        return FeedVisibility::Hidden;
    }

    let is_create = truthy(transaction.metadata_value("core.create"));
    if is_create {
        match kind {
            "core:create"
            | "core:view-policy"
            | "core:edit-policy"
            | "core:join-policy"
            | "core:interact-policy"
            | "core:space" => {}
            "core:subtype" => return FeedVisibility::Hidden,
            _ => {
                let old = &transaction.old_value;
                let empty = match old {
                    Value::Array(items) => items.is_empty(),
                    Value::Object(entries) => entries.is_empty(),
                    Value::Null => true,
                    Value::String(text) => text.is_empty(),
                    Value::Number(number) => number.as_i64() == Some(0) && !number.is_f64(),
                    Value::Bool(_) => false,
                };
                if empty {
                    return FeedVisibility::Hidden;
                }
            }
        }
        if truthy(transaction.metadata_value("core.default")) {
            return FeedVisibility::Hidden;
        }
    }

    match kind {
        "core:view-policy"
        | "core:edit-policy"
        | "core:join-policy"
        | "core:interact-policy"
        | "core:space"
            if !is_create =>
        {
            (transaction.old_value == Value::Null).into()
        }
        // No custom field overrides shouldHideInApplicationTransactions(),
        // and a field which doesn't exist hides nothing either.
        "core:customfield" => FeedVisibility::Visible,
        TYPE_EDGE => match edge_type(transaction) {
            Some(edges::OBJECT_MENTIONS_OBJECT | edges::MUTED | edges::MUTED_BY) => {
                FeedVisibility::Hidden
            }
            // Depends on whether the viewer can see the mentioning object.
            Some(edges::OBJECT_MENTIONED_BY_OBJECT) => FeedVisibility::Unknown,
            _ => FeedVisibility::Visible,
        },
        // Hidden by `shouldHideForFeed()` before reaching here.
        "core:inlinestate" => FeedVisibility::Unknown,
        _ => FeedVisibility::Visible,
    }
}

/// `PhabricatorApplicationTransaction::shouldHideForFeed()` up to its call
/// to `shouldHide()`, which every application's transactions share: `Some`
/// when it decides.
pub fn hidden_for_feed_before_should_hide(transaction: &Transaction) -> Option<FeedVisibility> {
    match is_self_subscription(transaction) {
        None => return Some(FeedVisibility::Unknown),
        Some(true) => return Some(FeedVisibility::Hidden),
        Some(false) => {}
    }
    match transaction.transaction_type.as_str() {
        "token:give" | "core:mfa" | "core:inlinestate" => Some(FeedVisibility::Hidden),
        TYPE_SUBSCRIBERS if transaction.author_phid.as_str().starts_with("PHID-APPS-") => {
            Some(FeedVisibility::Hidden)
        }
        TYPE_EDGE
            if matches!(
                edge_type(transaction),
                Some(
                    edges::OBJECT_MENTIONS_OBJECT
                        | edges::OBJECT_MENTIONED_BY_OBJECT
                        | edges::REVISION_DEPENDS_ON_REVISION
                        | edges::REVISION_DEPENDED_ON_BY_REVISION
                        | edges::COMMIT_HAS_REVISION
                        | edges::REVISION_HAS_COMMIT
                )
            ) =>
        {
            Some(FeedVisibility::Hidden)
        }
        _ => None,
    }
}

/// The `edge:type` of a transaction, which PHP compares loosely.
pub fn transaction_edge_type(transaction: &Transaction) -> Option<i64> {
    edge_type(transaction)
}

/// `shouldHideForFeed()` of a `DifferentialTransaction`, given its comment
/// if the viewer can see it.
pub fn feed_visibility(transaction: &Transaction, comment: Option<&Comment>) -> FeedVisibility {
    let kind = transaction.transaction_type.as_str();

    // The modular transaction type's own shouldHideForFeed().
    match kind {
        "harbormaster:buildable" => {
            if transaction.new_value.as_str() != Some("failed") {
                return FeedVisibility::Hidden;
            }
        }
        "differential.builds.wrong" | "differential.revision.wrong" => {
            return FeedVisibility::Hidden;
        }
        _ => {}
    }

    if let Some(decided) = hidden_for_feed_before_should_hide(transaction) {
        return decided;
    }

    // DifferentialTransaction::shouldHide(), then the modular type's.
    match kind {
        "differential.revision.request" => FeedVisibility::Visible,
        "differential.revision.void" => FeedVisibility::Hidden,
        _ => should_hide(transaction, comment),
    }
}

/// `shouldHideForFeed()` of a `PhabricatorAuditTransaction` (a commit's),
/// given its comment if the viewer can see it.
pub fn audit_feed_visibility(
    transaction: &Transaction,
    comment: Option<&Comment>,
) -> FeedVisibility {
    let kind = transaction.transaction_type.as_str();

    // DiffusionCommitBuildableTransaction::shouldHideForFeed(); the other
    // modular types keep the default.
    if kind == "harbormaster:buildable" && transaction.new_value.as_str() != Some("failed") {
        return FeedVisibility::Hidden;
    }
    if let Some(decided) = hidden_for_feed_before_should_hide(transaction) {
        return decided;
    }
    match kind {
        // Commit custom fields may hide their transactions.
        "core:customfield" => FeedVisibility::Unknown,
        _ => should_hide(transaction, comment),
    }
}

/// `shouldHideForFeed()` of a `PhabricatorProjectTransaction`, given its
/// comment if the viewer can see it: no modular type of projects hides
/// itself, and `shouldHide()` hides the silenced edges.
pub fn project_feed_visibility(
    transaction: &Transaction,
    comment: Option<&Comment>,
) -> FeedVisibility {
    if let Some(decided) = hidden_for_feed_before_should_hide(transaction) {
        return decided;
    }
    match transaction.transaction_type.as_str() {
        TYPE_EDGE if edge_type(transaction) == Some(edges::PROJECT_SILENCED) => {
            FeedVisibility::Hidden
        }
        // The description and the other fields of projects.
        "core:customfield" => FeedVisibility::Unknown,
        _ => should_hide(transaction, comment),
    }
}

/// `shouldHideForFeed()` of a `PhabricatorApplicationApplicationTransaction`,
/// given its comment if the viewer can see it: its only modular type,
/// `PhabricatorApplicationPolicyChangeTransaction`, keeps the default.
pub fn application_feed_visibility(
    transaction: &Transaction,
    comment: Option<&Comment>,
) -> FeedVisibility {
    hidden_for_feed_before_should_hide(transaction)
        .unwrap_or_else(|| should_hide(transaction, comment))
}

#[cfg(test)]
mod tests {
    use phab_core::Phid;
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn transaction(kind: &str, old: Value, new: Value, metadata: Value) -> Transaction {
        Transaction {
            phid: Phid::new("PHID-XACT-DREV-1"),
            author_phid: Phid::new("PHID-USER-alice"),
            object_phid: Phid::new("PHID-DREV-1"),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            comment_phid: None,
            transaction_type: kind.to_owned(),
            old_value: old,
            new_value: new,
            metadata,
        }
    }

    fn comment(content: &str) -> Comment {
        Comment {
            phid: Phid::new("PHID-XCMT-1"),
            author_phid: Phid::new("PHID-USER-alice"),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            content: content.to_owned(),
        }
    }

    #[rstest]
    #[case("differential.revision.accept", json!(null), json!(true), json!({}), FeedVisibility::Visible)]
    #[case("token:give", json!(null), json!("x"), json!({}), FeedVisibility::Hidden)]
    #[case("core:inlinestate", json!({}), json!({}), json!({}), FeedVisibility::Hidden)]
    #[case("core:subscribers", json!([]), json!(["PHID-USER-alice"]), json!({}), FeedVisibility::Hidden)]
    #[case("core:subscribers", json!([]), json!(["PHID-USER-bob"]), json!({}), FeedVisibility::Visible)]
    #[case("core:subscribers", json!([]), json!(["PHID-USER-alice", "PHID-USER-bob"]), json!({}), FeedVisibility::Visible)]
    #[case("core:subscribers", json!("bad"), json!([]), json!({}), FeedVisibility::Unknown)]
    #[case("core:edge", json!([]), json!([]), json!({"edge:type": 5}), FeedVisibility::Hidden)]
    #[case("core:edge", json!([]), json!([]), json!({"edge:type": "52"}), FeedVisibility::Hidden)]
    #[case("core:edge", json!([]), json!([]), json!({"edge:type": 41}), FeedVisibility::Visible)]
    #[case("core:customfield", json!(null), json!("x"), json!({"customfield:key": "std:x"}), FeedVisibility::Visible)]
    #[case("core:customfield", json!(""), json!("1"), json!({"customfield:key": "differential:bugzilla-bug-id"}), FeedVisibility::Visible)]
    #[case("core:view-policy", json!(null), json!("users"), json!({}), FeedVisibility::Hidden)]
    #[case("core:view-policy", json!("admin"), json!("users"), json!({}), FeedVisibility::Visible)]
    #[case("differential.revision.title", json!(""), json!("t"), json!({"core.create": true}), FeedVisibility::Hidden)]
    #[case("differential.revision.title", json!("a"), json!("t"), json!({"core.create": true}), FeedVisibility::Visible)]
    #[case("differential.revision.title", json!("a"), json!("t"), json!({"core.create": true, "core.default": true}), FeedVisibility::Hidden)]
    #[case("differential.revision.reviewers", json!([]), json!(["x"]), json!({"core.create": true}), FeedVisibility::Hidden)]
    #[case("differential.revision.request", json!(null), json!(true), json!({"core.create": true}), FeedVisibility::Visible)]
    #[case("differential.revision.void", json!(null), json!(true), json!({}), FeedVisibility::Hidden)]
    #[case("differential.builds.wrong", json!(null), json!(true), json!({}), FeedVisibility::Hidden)]
    #[case("harbormaster:buildable", json!(null), json!("passed"), json!({}), FeedVisibility::Hidden)]
    #[case("harbormaster:buildable", json!(null), json!("failed"), json!({}), FeedVisibility::Visible)]
    #[case("core:subtype", json!("a"), json!("b"), json!({"core.create": true}), FeedVisibility::Hidden)]
    #[case("core:file", json!([]), json!([]), json!({"attach.implicit": true}), FeedVisibility::Hidden)]
    fn transactions(
        #[case] kind: &str,
        #[case] old: Value,
        #[case] new: Value,
        #[case] metadata: Value,
        #[case] expected: FeedVisibility,
    ) {
        assert_eq!(
            feed_visibility(&transaction(kind, old, new, metadata), None),
            expected
        );
    }

    #[test]
    fn comments_make_transactions_visible() {
        let attach = transaction(
            "core:file",
            json!([]),
            json!([]),
            json!({"attach.implicit": true}),
        );
        assert_eq!(
            feed_visibility(&attach, Some(&comment("hello"))),
            FeedVisibility::Visible
        );
        assert_eq!(
            feed_visibility(&attach, Some(&comment(" \n\t"))),
            FeedVisibility::Hidden
        );
        let token = transaction("token:give", json!(null), json!("x"), json!({}));
        assert_eq!(
            feed_visibility(&token, Some(&comment("hello"))),
            FeedVisibility::Hidden
        );
    }

    #[test]
    fn commits_and_projects_hide_their_own_transactions() {
        let failed = transaction(
            "harbormaster:buildable",
            json!(null),
            json!("failed"),
            json!({}),
        );
        let passed = transaction(
            "harbormaster:buildable",
            json!(null),
            json!("passed"),
            json!({}),
        );
        let commit = transaction("audit:commit", json!(null), json!({}), json!({}));
        let field = transaction("core:customfield", json!(null), json!("x"), json!({}));
        assert_eq!(
            audit_feed_visibility(&failed, None),
            FeedVisibility::Visible
        );
        assert_eq!(audit_feed_visibility(&passed, None), FeedVisibility::Hidden);
        assert_eq!(
            audit_feed_visibility(&commit, None),
            FeedVisibility::Visible
        );
        assert_eq!(audit_feed_visibility(&field, None), FeedVisibility::Unknown);

        let silenced = transaction(
            "core:edge",
            json!([]),
            json!(["x"]),
            json!({"edge:type": 61}),
        );
        let member = transaction(
            "core:edge",
            json!([]),
            json!(["x"]),
            json!({"edge:type": 13}),
        );
        assert_eq!(
            project_feed_visibility(&silenced, None),
            FeedVisibility::Hidden
        );
        assert_eq!(
            project_feed_visibility(&member, None),
            FeedVisibility::Visible
        );
        assert_eq!(
            project_feed_visibility(&field, None),
            FeedVisibility::Unknown
        );
    }

    #[test]
    fn stories_show_if_any_transaction_shows() {
        use FeedVisibility::{Hidden, Unknown, Visible};
        assert_eq!(story_visibility(&[Hidden, Unknown, Visible]), Visible);
        assert_eq!(story_visibility(&[Hidden, Unknown]), Unknown);
        assert_eq!(story_visibility(&[Hidden, Hidden]), Hidden);
        assert_eq!(story_visibility(&[]), Hidden);
    }
}
