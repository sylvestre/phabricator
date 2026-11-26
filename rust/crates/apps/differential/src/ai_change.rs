//! `DifferentialAIChange` (moz-extensions, Bug 2091563): the AI change
//! state of a revision, which Lando requests an approval of after
//! uploading an AI's change as the active diff, and anyone but a bot may
//! approve. It lives in the revision's `mozilla.ai-change` property; a
//! newer diff than the one the request names makes the approval no longer
//! required ("lapsed").
//!
//! `differential.revision.search` returns it as `fields.aiChange`
//! (`DifferentialAIChangeSearchEngineExtension`). Stored values of a type
//! PHP's `idx()` would read differently (a property which isn't a
//! dictionary) are left to PHP.

use phab_conduit::MethodError;
use phab_core::php;
use serde_json::{Map, Value, json};

/// `DifferentialAIChange::PROPERTY`.
pub const PROPERTY: &str = "mozilla.ai-change";

/// The `fields` key of `differential.revision.search`.
pub const CONDUIT_KEY: &str = "aiChange";

// The keys of the stored property.
pub(crate) const KEY_KIND: &str = "kind";
pub(crate) const KEY_CHANGE_DIFF_ID: &str = "changeDiffID";
pub(crate) const KEY_CHANGE_DIFF_PHID: &str = "changeDiffPHID";
pub(crate) const KEY_BASE_DIFF_ID: &str = "baseDiffID";
pub(crate) const KEY_SUMMARY: &str = "summary";
pub(crate) const KEY_CONFIDENCE: &str = "confidence";
pub(crate) const KEY_DETAILS: &str = "details";
pub(crate) const KEY_REQUESTER_PHID: &str = "requesterPHID";
pub(crate) const KEY_REQUEST_EPOCH: &str = "requestEpoch";
pub(crate) const KEY_APPROVER_PHID: &str = "approverPHID";
pub(crate) const KEY_APPROVAL_EPOCH: &str = "approvalEpoch";

/// `KIND_MERGE_CONFLICT`, `KIND_REQUESTED_EDIT` and their names
/// (`getKindNames()`), in PHP's order.
pub const KINDS: &[(&str, &str)] = &[
    ("merge-conflict", "AI merge conflict resolution"),
    ("requested-edit", "AI edit"),
];

/// `getKindName()`: the noun for a kind, "AI change" for any other value.
pub fn kind_name(kind: Option<&Value>) -> &'static str {
    KINDS
        .iter()
        .find(|(known, _)| kind.and_then(Value::as_str) == Some(*known))
        .map_or("AI change", |(_, name)| name)
}

/// `getStatus()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No request.
    None,
    /// Requested on the active diff, not approved.
    Pending,
    /// Approved, on the active diff.
    Approved,
    /// Requested on another diff than the active one.
    Lapsed,
}

impl Status {
    /// `STATUS_*`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Lapsed => "lapsed",
        }
    }
}

/// The AI change of a revision: its stored property.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AiChange {
    data: Map<String, Value>,
}

impl AiChange {
    /// `newFromRevision()` of the decoded `properties`: `getProperty()`
    /// with `array()` as the default. A falsy value has no AI change; one
    /// which isn't a dictionary is left to PHP.
    pub fn from_properties(properties: &Value) -> Result<Self, MethodError> {
        match properties.get(PROPERTY) {
            Some(Value::Object(data)) => Ok(Self { data: data.clone() }),
            None => Ok(Self::default()),
            Some(value) if !php::is_truthy(value) => Ok(Self::default()),
            Some(_) => Err(MethodError::Unsupported("an AI change of an unusual type")),
        }
    }

    /// The stored value of `key`, `null` when missing (`idx()`).
    pub fn value(&self, key: &str) -> Value {
        self.data.get(key).cloned().unwrap_or(Value::Null)
    }

    /// The stored value of `key`, if any and not `null`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key).filter(|value| !value.is_null())
    }

    /// `getStatus()` for the revision's `activeDiffPHID`: a newer diff
    /// than the change's lapses it (the comparison is strict).
    pub fn status(&self, active_diff_phid: Option<&str>) -> Status {
        if self.data.is_empty() {
            return Status::None;
        }
        let same = match (active_diff_phid, self.data.get(KEY_CHANGE_DIFF_PHID)) {
            (None, None | Some(Value::Null)) => true,
            (Some(active), Some(Value::String(change))) => active == change,
            _ => false,
        };
        if !same {
            return Status::Lapsed;
        }
        if self.data.get(KEY_APPROVER_PHID).is_some_and(php::is_truthy) {
            return Status::Approved;
        }
        Status::Pending
    }

    /// The `aiChange` field of `differential.revision.search`.
    pub fn conduit_value(&self, active_diff_phid: Option<&str>) -> Value {
        json!({
            "status": self.status(active_diff_phid).as_str(),
            "kind": self.value(KEY_KIND),
            "changeDiffID": self.value(KEY_CHANGE_DIFF_ID),
            "baseDiffID": self.value(KEY_BASE_DIFF_ID),
            "requesterPHID": self.value(KEY_REQUESTER_PHID),
            "requestEpoch": self.value(KEY_REQUEST_EPOCH),
            "approverPHID": self.value(KEY_APPROVER_PHID),
            "approvalEpoch": self.value(KEY_APPROVAL_EPOCH),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(data: &Value) -> AiChange {
        AiChange::from_properties(&json!({PROPERTY: data})).unwrap()
    }

    #[test]
    fn status_follows_php() {
        let none = AiChange::from_properties(&json!({})).unwrap();
        assert_eq!(none.status(Some("PHID-DIFF-a")), Status::None);
        assert_eq!(change(&json!([])).status(Some("PHID-DIFF-a")), Status::None);
        assert_eq!(
            change(&json!(null)).status(Some("PHID-DIFF-a")),
            Status::None
        );
        assert!(AiChange::from_properties(&json!({PROPERTY: "x"})).is_err());

        let pending = change(&json!({"changeDiffPHID": "PHID-DIFF-a", "approverPHID": null}));
        assert_eq!(pending.status(Some("PHID-DIFF-a")), Status::Pending);
        assert_eq!(pending.status(Some("PHID-DIFF-b")), Status::Lapsed);
        let approved =
            change(&json!({"changeDiffPHID": "PHID-DIFF-a", "approverPHID": "PHID-USER-x"}));
        assert_eq!(approved.status(Some("PHID-DIFF-a")), Status::Approved);
        assert_eq!(approved.status(Some("PHID-DIFF-b")), Status::Lapsed);
    }

    #[test]
    fn conduit_value_follows_php() {
        assert_eq!(
            AiChange::default().conduit_value(Some("PHID-DIFF-a")),
            json!({
                "status": "none",
                "kind": null,
                "changeDiffID": null,
                "baseDiffID": null,
                "requesterPHID": null,
                "requestEpoch": null,
                "approverPHID": null,
                "approvalEpoch": null,
            })
        );
        let pending = change(&json!({
            "kind": "merge-conflict",
            "changeDiffID": 2,
            "changeDiffPHID": "PHID-DIFF-a",
            "baseDiffID": 1,
            "summary": "Kept the beta API.",
            "requesterPHID": "PHID-USER-l",
            "requestEpoch": 1_700_000_000,
            "approverPHID": null,
            "approvalEpoch": null,
        }));
        assert_eq!(
            pending.conduit_value(Some("PHID-DIFF-a")),
            json!({
                "status": "pending",
                "kind": "merge-conflict",
                "changeDiffID": 2,
                "baseDiffID": 1,
                "requesterPHID": "PHID-USER-l",
                "requestEpoch": 1_700_000_000,
                "approverPHID": null,
                "approvalEpoch": null,
            })
        );
    }

    #[test]
    fn kind_names_follow_php() {
        assert_eq!(
            kind_name(Some(&json!("merge-conflict"))),
            "AI merge conflict resolution"
        );
        assert_eq!(kind_name(Some(&json!("requested-edit"))), "AI edit");
        assert_eq!(kind_name(Some(&json!("vibes"))), "AI change");
        assert_eq!(kind_name(None), "AI change");
    }
}
