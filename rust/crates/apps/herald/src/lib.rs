//! Herald rules (`HeraldRule`) and spaces (`PhabricatorSpacesNamespace`),
//! as references to them (`H123`, `S1`) show them.

pub mod editor;
pub mod engine;
pub mod spaces;
pub mod web;

use std::collections::HashSet;
use std::hash::BuildHasher;

use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, POLICY_PUBLIC, POLICY_USER, PolicySubject, Viewer};

/// `HeraldRuleTypeConfig::RULE_TYPE_OBJECT`.
const RULE_TYPE_OBJECT: &str = "object";

/// A `herald_rule` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The row ID.
    pub id: i64,
    /// The rule PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// The adapter, like `differential`.
    pub content_type: String,
    /// `global`, `personal` or `object`.
    pub rule_type: String,
    /// `isDisabled`.
    pub is_disabled: bool,
    /// The project or repository of an object rule.
    pub trigger_object_phid: Option<Phid>,
}

impl Rule {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            content_type: rows::text(row, "contentType")?,
            rule_type: rows::text(row, "ruleType")?,
            is_disabled: rows::flag(row, "isDisabled")?,
            trigger_object_phid: rows::opt_phid(row, "triggerObjectPHID")?,
        })
    }

    /// `getMonogram()`.
    pub fn monogram(&self) -> String {
        format!("H{}", self.id)
    }

    /// `isObjectRule()`.
    pub fn is_object_rule(&self) -> bool {
        self.rule_type == RULE_TYPE_OBJECT
    }
}

/// The application of a Herald adapter (`getAdapterApplicationClass()`),
/// by content type; unknown adapters are not enabled.
fn adapter_application(content_type: &str) -> Option<&'static str> {
    Some(match content_type {
        "differential" | "differential.diff" => "PhabricatorDifferentialApplication",
        "commit" | "HeraldPreCommitRefAdapter" | "HeraldPreCommitContentAdapter" => {
            "PhabricatorDiffusionApplication"
        }
        "PhabricatorProjectHeraldAdapter" => "PhabricatorProjectApplication",
        "PhabricatorMailOutboundMailHeraldAdapter" => "PhabricatorMetaMTAApplication",
        "PhabricatorCalendarEventHeraldAdapter" => "PhabricatorCalendarApplication",
        "HeraldRuleAdapter" => phab_policy::application::HERALD,
        _ => return None,
    })
}

/// Why visibility could not be decided.
#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    /// Loading the policy facts failed.
    #[error(transparent)]
    Loader(#[from] phab_policy::loader::LoaderError),
    /// The policy filter refused the check.
    #[error(transparent)]
    Policy(#[from] phab_policy::PolicyError),
}

/// The rules with these IDs; unknown IDs are skipped.
pub async fn load_by_ids(database: &Database, ids: &[i64]) -> Result<Vec<Rule>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, phid, name, contentType, ruleType, isDisabled, triggerObjectPHID \
         FROM {} WHERE id IN ({})",
        database.table("herald", "herald_rule"),
        phab_db::placeholders(ids.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), ids)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Rule::from_row)
        .collect()
}

/// The rules `viewer` can see (`HeraldRuleQuery`): their adapter's
/// application is visible to them, an object rule's object is among
/// `visible_triggers`, and everyone sees rules (`getMostOpenPolicy()`)
/// who sees Herald.
pub async fn visible<H: BuildHasher>(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    rules: Vec<Rule>,
    visible_triggers: &HashSet<Phid, H>,
    now: i64,
) -> Result<Vec<Rule>, VisibilityError> {
    let most_open = if config.allow_public() {
        POLICY_PUBLIC
    } else {
        POLICY_USER
    };
    let subjects: Vec<PolicySubject> = rules
        .iter()
        .map(|rule| {
            PolicySubject::new(Some(rule.phid.clone()), ObjectKind::Other)
                .with_policy(Capability::VIEW, most_open)
        })
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts =
        phab_policy::loader::load_facts(database, config, viewer, &refs, &NoExtendedObjects, now)
            .await?;
    let passed = phab_policy::filter(viewer, &refs, &[Capability::VIEW], &facts)?;
    let mut visible = Vec::new();
    for (rule, passed) in rules.into_iter().zip(passed) {
        let Some(application) = adapter_application(&rule.content_type) else {
            continue;
        };
        if !phab_policy::filter::can_see_application(viewer, application, &facts)? {
            continue;
        }
        if rule.is_object_rule()
            && !rule
                .trigger_object_phid
                .as_ref()
                .is_some_and(|phid| visible_triggers.contains(phid))
        {
            continue;
        }
        if passed {
            visible.push(rule);
        }
    }
    Ok(visible)
}
