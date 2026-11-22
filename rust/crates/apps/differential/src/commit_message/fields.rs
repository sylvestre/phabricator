//! The `DifferentialCommitMessageField` subclasses, those of Mozilla
//! included (`DifferentialBugzillaBugIDCommitMessageField`), and which of
//! them `newEnabledFields()` enables, in its order.

use phab_conduit::MethodError;
use phab_core::{Config, php};
use serde_json::{Map, Value};

/// `DifferentialTitleCommitMessageField::getDefaultTitle()`.
pub(super) const DEFAULT_TITLE: &str = "<<Replace this line with your revision title>";

/// A commit message field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Title,
    Summary,
    TestPlan,
    Reviewers,
    ReviewedBy,
    Subscribers,
    Tags,
    Auditors,
    BugId,
    BlameRevision,
    RevertPlan,
    RevisionId,
    Conflicts,
    GitSvnId,
}

/// What the value of a field is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A string or null (`readStringFieldValueFromConduit()`).
    Text,
    /// A list of PHIDs (`readStringListFieldValueFromConduit()`).
    List,
    /// The revision ID, an integer when parsed.
    RevisionId,
}

const ALL: [Field; 14] = [
    Field::Title,
    Field::Summary,
    Field::TestPlan,
    Field::Reviewers,
    Field::ReviewedBy,
    Field::Subscribers,
    Field::Tags,
    Field::Auditors,
    Field::BugId,
    Field::BlameRevision,
    Field::RevertPlan,
    Field::RevisionId,
    Field::Conflicts,
    Field::GitSvnId,
];

/// The `DifferentialCustomField`s the enabled commit message fields depend
/// on, in the order of the class map (`buildFieldList()`), with
/// `shouldDisableByDefault()` and `canDisableField()`. The others only
/// shift every custom field's order by the same amount.
const CUSTOM_FIELDS: [(&str, bool, bool); 5] = [
    // DifferentialAuditorsField
    ("phabricator:auditors", false, false),
    // DifferentialBlameRevisionField
    ("phabricator:blame-revision", true, true),
    // DifferentialBugzillaBugIDField
    ("differential:bugzilla-bug-id", false, false),
    // DifferentialRevertPlanField
    ("phabricator:revert-plan", true, true),
    // DifferentialTestPlanField
    ("differential:test-plan", false, true),
];

/// The keys of the default `differential.fields`, in its order, among
/// [`CUSTOM_FIELDS`].
const DEFAULT_SPEC: [&str; 3] = [
    "differential:test-plan",
    "phabricator:blame-revision",
    "phabricator:revert-plan",
];

impl Field {
    /// `getCommitMessageFieldKey()`.
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Summary => "summary",
            Self::TestPlan => "testPlan",
            Self::Reviewers => "reviewerPHIDs",
            Self::ReviewedBy => "reviewedByPHIDs",
            Self::Subscribers => "ccPHIDs",
            Self::Tags => "phabricator:projects",
            Self::Auditors => "phabricator:auditors",
            Self::BugId => "bugzilla.bug-id",
            Self::BlameRevision => "blameRevision",
            Self::RevertPlan => "revertPlan",
            Self::RevisionId => "revisionID",
            Self::Conflicts => "conflicts",
            Self::GitSvnId => "gitSVNID",
        }
    }

    /// `getFieldName()`, the label.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Summary => "Summary",
            Self::TestPlan => "Test Plan",
            Self::Reviewers => "Reviewers",
            Self::ReviewedBy => "Reviewed By",
            Self::Subscribers => "Subscribers",
            Self::Tags => "Tags",
            Self::Auditors => "Auditors",
            Self::BugId => "Bug #",
            Self::BlameRevision => "Blame Revision",
            Self::RevertPlan => "Revert Plan",
            Self::RevisionId => "Differential Revision",
            Self::Conflicts => "Conflicts",
            Self::GitSvnId => "git-svn-id",
        }
    }

    /// `getFieldAliases()`.
    pub(super) fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::TestPlan => &["Testplan", "Tested", "Tests"],
            Self::Reviewers => &["Reviewer"],
            Self::Subscribers => &["CC", "CCs", "Subscriber"],
            Self::Tags => &["Tag", "Project", "Projects"],
            Self::Auditors => &["Auditor"],
            Self::BugId => &["Bugzilla Bug ID", "Bugzilla"],
            Self::BlameRevision => &["Blame Rev"],
            _ => &[],
        }
    }

    /// `isFieldEditable()`.
    pub(super) fn is_editable(self) -> bool {
        !matches!(self, Self::ReviewedBy | Self::Conflicts | Self::GitSvnId)
    }

    /// `isTemplateField()`.
    pub(super) fn is_template(self) -> bool {
        !matches!(
            self,
            Self::ReviewedBy
                | Self::Tags
                | Self::Auditors
                | Self::RevisionId
                | Self::Conflicts
                | Self::GitSvnId
        )
    }

    pub(super) fn kind(self) -> Kind {
        match self {
            Self::Reviewers
            | Self::ReviewedBy
            | Self::Subscribers
            | Self::Tags
            | Self::Auditors => Kind::List,
            Self::RevisionId => Kind::RevisionId,
            _ => Kind::Text,
        }
    }

    /// `getCustomFieldKey()` of the `DifferentialCommitMessageCustomField`s.
    pub(super) fn custom_key(self) -> Option<&'static str> {
        match self {
            Self::Auditors => Some("phabricator:auditors"),
            Self::BugId => Some("differential:bugzilla-bug-id"),
            Self::BlameRevision => Some("phabricator:blame-revision"),
            Self::RevertPlan => Some("phabricator:revert-plan"),
            _ => None,
        }
    }

    /// `getFieldOrder()` of the fields which are not custom fields.
    fn fixed_order(self) -> i64 {
        match self {
            Self::Title => 1000,
            Self::Summary => 2000,
            Self::TestPlan => 3000,
            Self::Reviewers => 4000,
            Self::ReviewedBy => 5000,
            Self::Subscribers => 6000,
            Self::Tags => 7000,
            Self::RevisionId => 200_000,
            Self::Conflicts => 900_000,
            _ => 900_001,
        }
    }
}

/// `PhabricatorCustomField::getObjectFields(..., ROLE_DEFAULT)` among
/// [`CUSTOM_FIELDS`]: the keys `differential.fields` lists come first, in
/// its order, then the others; disabled ones are left out.
fn enabled_custom_fields(config: &Config) -> Result<Vec<&'static str>, MethodError> {
    let empty = Map::new();
    let configured = match config.value("differential.fields") {
        None => None,
        Some(Value::Object(spec)) => Some(spec),
        // An empty PHP array.
        Some(Value::Array(list)) if list.is_empty() => Some(&empty),
        Some(_) => return Err(MethodError::Unsupported("an unusual differential.fields")),
    };
    let listed: Vec<&'static str> = match configured {
        None => DEFAULT_SPEC.to_vec(),
        Some(spec) => spec
            .keys()
            .filter_map(|key| {
                CUSTOM_FIELDS
                    .iter()
                    .find(|(known, _, _)| known == key)
                    .map(|(known, _, _)| *known)
            })
            .collect(),
    };
    let mut keys = listed;
    for (key, _, _) in CUSTOM_FIELDS {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.retain(|key| {
        let &(_, by_default, can_disable) = CUSTOM_FIELDS
            .iter()
            .find(|(known, _, _)| known == key)
            .expect("a known custom field");
        // `isset($spec[$key]['disabled'])`; the default specification has
        // the defaults.
        let disabled = configured
            .and_then(|spec| spec.get(*key))
            .and_then(Value::as_object)
            .and_then(|entry| entry.get("disabled"))
            .filter(|value| !value.is_null())
            .map_or(by_default, php::is_truthy);
        !(disabled && can_disable)
    });
    Ok(keys)
}

/// `DifferentialCommitMessageField::newEnabledFields()`, sorted by
/// `getFieldOrder()`: custom fields come after the tags, in the order of
/// the custom field list.
pub(super) fn enabled_fields(config: &Config) -> Result<Vec<Field>, MethodError> {
    let custom = enabled_custom_fields(config)?;
    let mut fields: Vec<(i64, Field)> = ALL
        .into_iter()
        .filter_map(|field| {
            let order = match field.custom_key() {
                Some(key) => {
                    100_000 + i64::try_from(custom.iter().position(|k| *k == key)?).ok()?
                }
                None if field == Field::TestPlan => {
                    custom.contains(&"differential:test-plan").then_some(())?;
                    field.fixed_order()
                }
                None => field.fixed_order(),
            };
            Some((order, field))
        })
        .collect();
    fields.sort_by_key(|(order, _)| *order);
    Ok(fields.into_iter().map(|(_, field)| field).collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(fields: &Value) -> Config {
        let mut config = Config::default();
        if let Value::Object(map) = json!({"differential.fields": fields}) {
            config.push_layer("test", map);
        }
        config
    }

    #[test]
    fn enables_the_default_fields() {
        let fields = enabled_fields(&Config::default()).unwrap();
        assert_eq!(
            fields,
            [
                Field::Title,
                Field::Summary,
                Field::TestPlan,
                Field::Reviewers,
                Field::ReviewedBy,
                Field::Subscribers,
                Field::Tags,
                Field::Auditors,
                Field::BugId,
                Field::RevisionId,
                Field::Conflicts,
                Field::GitSvnId,
            ]
        );
    }

    #[test]
    fn follows_differential_fields() {
        let fields = enabled_fields(&config(&json!({
            "differential:bugzilla-bug-id": {"disabled": false},
            "differential:test-plan": {"disabled": true},
            "phabricator:revert-plan": {"disabled": false},
            // Can't be disabled.
            "phabricator:auditors": {"disabled": true},
        })))
        .unwrap();
        assert_eq!(
            fields,
            [
                Field::Title,
                Field::Summary,
                Field::Reviewers,
                Field::ReviewedBy,
                Field::Subscribers,
                Field::Tags,
                Field::BugId,
                Field::RevertPlan,
                Field::Auditors,
                Field::RevisionId,
                Field::Conflicts,
                Field::GitSvnId,
            ]
        );
        assert!(enabled_fields(&config(&json!("x"))).is_err());
        assert!(
            enabled_fields(&config(&json!([])))
                .unwrap()
                .contains(&Field::TestPlan)
        );
    }
}
