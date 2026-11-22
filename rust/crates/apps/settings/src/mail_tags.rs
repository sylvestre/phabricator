//! The mail tags of `PhabricatorEmailPreferencesSettingsPanel`: the
//! editors which have some (`getMailTagsMap()`), in the order PHP's class
//! map lists them (by class name).

use serde_json::{Map, Value, json};

use crate::Error;
use crate::preferences::int_cast;

/// `PhabricatorEmailTagsSetting::VALUE_EMAIL`, what a tag without a
/// preference gets.
pub const VALUE_EMAIL: i64 = 1;

/// The choices of a tag, in the order of the select.
pub const CHOICES: &[(i64, &str)] = &[
    (VALUE_EMAIL, "\u{26ab} Email"),
    (0, "\u{25d0} Notify"),
    (2, "\u{26aa} Ignore"),
];

/// An editor with mail tags.
#[derive(Debug, Clone, Copy)]
pub struct Editor {
    /// `getEditorApplicationClass()`: its tags exist only while the
    /// application is installed and visible.
    pub application: &'static str,
    /// `getEditorObjectsDescription()`, the label of its tags.
    pub description: &'static str,
    /// The tags and what they are about.
    pub tags: &'static [(&'static str, &'static str)],
}

/// The editors with mail tags, by class name: `DifferentialTransactionEditor`,
/// `PhabricatorAuditEditor`, `PhabricatorCalendarEventEditor`,
/// `PhabricatorProjectTransactionEditor`.
pub const EDITORS: &[Editor] = &[
    Editor {
        application: "PhabricatorDifferentialApplication",
        description: "Differential Revisions",
        tags: &[
            ("differential-review-request", "A revision is created."),
            ("differential-updated", "A revision is updated."),
            ("differential-comment", "Someone comments on a revision."),
            ("differential-committed", "A revision is closed."),
            ("differential-reviewers", "A revision's reviewers change."),
            ("differential-cc", "A revision's CCs change."),
            (
                "differential-other",
                "Other revision activity not listed above occurs.",
            ),
        ],
    },
    Editor {
        application: "PhabricatorDiffusionApplication",
        description: "Audits",
        tags: &[
            ("audit-commit", "A commit is created."),
            (
                "audit-action-concern",
                "A commit has a concerned raised against it.",
            ),
            ("audit-action-accept", "A commit is accepted."),
            ("audit-action-resign", "A commit has an auditor resign."),
            ("audit-action-close", "A commit is closed."),
            ("audit-add-auditors", "A commit has auditors added."),
            ("audit-add-ccs", "A commit's subscribers change."),
            ("audit-projects", "A commit's projects change."),
            ("audit-comment", "Someone comments on a commit."),
            (
                "audit-other",
                "Other commit activity not listed above occurs.",
            ),
        ],
    },
    Editor {
        application: "PhabricatorCalendarApplication",
        description: "Calendar",
        tags: &[
            (
                "calendar-content",
                "An event's name, status, invite list, icon, and description changes.",
            ),
            (
                "calendar-reschedule",
                "An event's start and end date and cancellation status changes.",
            ),
            (
                "calendar-other",
                "Other event activity not listed above occurs.",
            ),
        ],
    },
    Editor {
        application: "PhabricatorProjectApplication",
        description: "Projects",
        tags: &[
            (
                "project-metadata",
                "Project name, hashtags, icon, image, or color changes.",
            ),
            ("project-members", "Project membership changes."),
            ("project-watchers", "Project watcher list changes."),
            (
                "project-other",
                "Other project activity not listed above occurs.",
            ),
        ],
    },
];

/// A tag as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// The tag key, posted as `mailtags[key]`.
    pub key: &'static str,
    /// What it is about.
    pub label: &'static str,
    /// The current choice.
    pub value: i64,
}

/// The tags of one editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagGroup {
    /// The editor's description.
    pub label: &'static str,
    /// Its tags.
    pub tags: Vec<Tag>,
}

/// The current choice of `key` in `preferences` (the `mailtags`
/// setting, or its default): `(int)idx($prefs, $key, VALUE_EMAIL)`.
fn choice(preferences: &Value, key: &str) -> i64 {
    let stored = match preferences {
        Value::Object(map) => map.get(key),
        _ => None,
    };
    stored.map_or(VALUE_EMAIL, int_cast)
}

/// The groups of the panel, sorted by label (PHP's "Common" group never
/// has any tag: no tag is counted twice). `editors` are the editors of
/// installed applications.
pub fn groups(editors: &[&Editor], preferences: &Value) -> Vec<TagGroup> {
    let mut groups: Vec<TagGroup> = editors
        .iter()
        .map(|editor| TagGroup {
            label: editor.description,
            tags: editor
                .tags
                .iter()
                .map(|(key, label)| Tag {
                    key,
                    label,
                    value: choice(preferences, key),
                })
                .collect(),
        })
        .collect();
    groups.sort_by_key(|group| group.label);
    groups
}

/// The `mailtags` the form saves: the stored ones, each tag of an
/// installed application set to its posted value (or to email).
/// `posted` are the `mailtags[...]` fields; `stored` is the stored setting.
pub fn new_value(
    editors: &[&Editor],
    stored: &Value,
    posted: &[(String, String)],
) -> Result<Value, Error> {
    let mut tags: Map<String, Value> = match stored {
        // `$mailtags[$key] = ...` creates the array.
        Value::Null => Map::new(),
        Value::Array(list) => list
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value.clone()))
            .collect(),
        Value::Object(map) => map.clone(),
        _ => return Err(Error::Unsupported("mail tags which are not an array")),
    };
    for editor in editors {
        for (key, _) in editor.tags {
            let value = posted
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map_or(VALUE_EMAIL, |(_, value)| {
                    crate::preferences::int_cast_str(value)
                });
            tags.insert((*key).to_owned(), json!(value));
        }
    }
    Ok(Value::Object(tags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_sorted_and_default_to_email() {
        let editors: Vec<&Editor> = EDITORS.iter().collect();
        let groups = groups(
            &editors,
            &json!({"differential-cc": "2", "audit-commit": 0}),
        );
        let labels: Vec<_> = groups.iter().map(|group| group.label).collect();
        assert_eq!(
            labels,
            ["Audits", "Calendar", "Differential Revisions", "Projects"]
        );
        assert_eq!(groups[0].tags[0].value, 0);
        assert_eq!(groups[2].tags[5].value, 2);
        assert_eq!(groups[2].tags[0].value, VALUE_EMAIL);
    }

    #[test]
    fn saved_tags_keep_the_stored_order() {
        let editors = [&EDITORS[3]];
        let posted = [
            ("project-members".to_owned(), "2".to_owned()),
            ("unknown".to_owned(), "0".to_owned()),
        ];
        let value = new_value(&editors, &json!({"x": 5, "project-other": 0}), &posted).unwrap();
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "x",
                "project-other",
                "project-metadata",
                "project-members",
                "project-watchers"
            ]
        );
        assert_eq!(value["project-members"], json!(2));
        assert_eq!(value["project-other"], json!(1));
        assert!(new_value(&editors, &json!("text"), &posted).is_err());
    }
}
