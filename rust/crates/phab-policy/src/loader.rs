//! Loads [`PolicyFacts`] from the database, doing the queries PHP runs from
//! `PhabricatorPolicyFilter` and the rules' `willApplyRules()`.

use std::collections::HashMap;

use phab_core::{Config, Phid, PhidType};

use crate::{
    ApplicationPolicies, Capability, CustomPolicy, ObjectKind, PolicyFacts, PolicySubject, Space,
    Viewer, application,
};
use phab_db::{Database, DbError, edges, rows};
use thiserror::Error;

const MAX_RESOLVE_ROUNDS: usize = 32;

/// A failure while loading [`PolicyFacts`].
#[derive(Debug, Error)]
pub enum LoaderError {
    /// A query failed.
    #[error(transparent)]
    Database(#[from] DbError),
}

impl From<phab_db::sqlx::Error> for LoaderError {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Database(error.into())
    }
}

/// Loads the objects extended policies point to (for example the repository
/// of a revision), as seen by the omnipotent viewer.
pub trait SubjectResolver {
    /// Loads the subjects for `phids`. PHIDs that are missing from the result
    /// make the extended policies naming them fail.
    fn resolve(
        &self,
        phids: &[Phid],
    ) -> impl Future<Output = Result<HashMap<Phid, PolicySubject>, LoaderError>> + Send;
}

/// A resolver for callers whose objects have no extended policies.
pub struct NoExtendedObjects;

impl SubjectResolver for NoExtendedObjects {
    async fn resolve(&self, _phids: &[Phid]) -> Result<HashMap<Phid, PolicySubject>, LoaderError> {
        Ok(HashMap::new())
    }
}

/// Loads what [`crate::filter`] needs to decide on `subjects` for `viewer`,
/// including the objects reached through extended policies, spaces and
/// application policies. Returns early, with only configuration, for the
/// omnipotent viewer, and without membership data for a logged-out one.
///
/// # Errors
///
/// [`LoaderError`] when a query or the `resolver` fails.
pub async fn load_facts(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    subjects: &[&PolicySubject],
    resolver: &impl SubjectResolver,
    now: i64,
) -> Result<PolicyFacts, LoaderError> {
    let mut facts = PolicyFacts {
        allow_public: config.allow_public(),
        applications: ApplicationPolicies::from_config(config),
        now,
        ..PolicyFacts::default()
    };
    if viewer.is_omnipotent {
        return Ok(facts);
    }

    resolve_extended_objects(subjects, resolver, &mut facts).await?;
    facts.spaces = load_spaces(database).await?;

    let mut all: Vec<&PolicySubject> = subjects.to_vec();
    all.extend(facts.extended_objects.values());
    all.extend(facts.spaces.values().map(|space| &space.subject));

    let application_subjects: Vec<PolicySubject> = {
        let mut classes: Vec<&str> = all
            .iter()
            .filter_map(|subject| {
                application::application_for_phid_type(subject.phid.as_ref()?.phid_type())
            })
            .collect();
        classes.sort_unstable();
        classes.dedup();
        classes
            .into_iter()
            .map(|class| facts.applications.subject(class, facts.allow_public))
            .collect()
    };
    all.extend(application_subjects.iter());

    let mut policy_phids: Vec<Phid> = all
        .iter()
        .flat_map(|subject| subject.policies.values())
        .map(|policy| Phid::new(policy.as_str()))
        .filter(|phid| phid.phid_type() == PhidType::POLICY)
        .collect();
    policy_phids.sort();
    policy_phids.dedup();
    facts.custom_policies = load_custom_policies(database, &policy_phids).await?;

    let Some(viewer_phid) = viewer.phid.clone() else {
        return Ok(facts);
    };

    facts.viewer_projects = load_materialized_projects(database, &viewer_phid)
        .await?
        .into_iter()
        .collect();

    let mut sources = vec![viewer_phid.clone()];
    sources.extend(facts.viewer_projects.iter().cloned());

    let revisions: Vec<Phid> = all
        .iter()
        .filter(|subject| subject.kind == ObjectKind::Revision)
        .filter_map(|subject| subject.phid.clone())
        .collect();
    if !revisions.is_empty() {
        facts.reviewing = load_reviewing(database, &revisions, &sources)
            .await?
            .into_iter()
            .collect();
    }

    let subscribable: Vec<Phid> = all
        .iter()
        .filter(|subject| subject.subscribable)
        .filter_map(|subject| subject.phid.clone())
        .collect();
    if !subscribable.is_empty() {
        facts.subscribed = edges::load_destination_phids(
            database,
            &sources,
            &[edges::SUBSCRIBED_TO_OBJECT],
            Some(&subscribable),
        )
        .await?
        .into_iter()
        .collect();
    }

    let projects: Vec<Phid> = all
        .iter()
        .filter(|subject| subject.kind == ObjectKind::Project)
        .filter_map(|subject| subject.phid.clone())
        .collect();
    if !projects.is_empty() {
        facts.viewer_direct_projects = edges::load_destination_phids(
            database,
            &[viewer_phid],
            &[edges::MEMBER_OF_PROJECT],
            Some(&projects),
        )
        .await?
        .into_iter()
        .collect();
    }

    Ok(facts)
}

async fn resolve_extended_objects(
    subjects: &[&PolicySubject],
    resolver: &impl SubjectResolver,
    facts: &mut PolicyFacts,
) -> Result<(), LoaderError> {
    let mut pending: Vec<Phid> = subjects
        .iter()
        .flat_map(|subject| {
            subject
                .extended
                .iter()
                .map(|extended| extended.object.clone())
        })
        .collect();
    for _ in 0..MAX_RESOLVE_ROUNDS {
        pending.sort();
        pending.dedup();
        pending.retain(|phid| !facts.extended_objects.contains_key(phid));
        if pending.is_empty() {
            break;
        }
        let loaded = resolver.resolve(&pending).await?;
        pending = loaded
            .values()
            .flat_map(|subject| {
                subject
                    .extended
                    .iter()
                    .map(|extended| extended.object.clone())
            })
            .collect();
        facts.extended_objects.extend(loaded);
    }
    Ok(())
}

/// Projects the viewer is a member of, including through subprojects
/// (`PhabricatorProjectQuery::withMemberPHIDs()`).
async fn load_materialized_projects(
    database: &Database,
    viewer_phid: &Phid,
) -> Result<Vec<Phid>, LoaderError> {
    let sql = format!(
        "SELECT project.phid FROM {} project JOIN {} e \
         ON e.src = project.phid AND e.type = ? WHERE e.dst = ?",
        database.table("project", "project"),
        database.table("project", "edge"),
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(edges::PROJECT_MATERIALIZED_MEMBER)
        .bind(viewer_phid.as_str())
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok(Phid::new(rows::text(row, "phid")?)))
        .collect()
}

async fn load_reviewing(
    database: &Database,
    revisions: &[Phid],
    reviewers: &[Phid],
) -> Result<Vec<Phid>, LoaderError> {
    let sql = format!(
        "SELECT revisionPHID FROM {} WHERE revisionPHID IN ({}) AND reviewerPHID IN ({})",
        database.table("differential", "differential_reviewer"),
        phab_db::placeholders(revisions.len()),
        phab_db::placeholders(reviewers.len()),
    );
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(Phid::as_str),
    );
    let rows = phab_db::bind_all(query, reviewers.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok(Phid::new(rows::text(row, "revisionPHID")?)))
        .collect()
}

async fn load_custom_policies(
    database: &Database,
    phids: &[Phid],
) -> Result<HashMap<Phid, CustomPolicy>, LoaderError> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT phid, rules, defaultAction FROM {} WHERE phid IN ({})",
        database.table("policy", "policy"),
        phab_db::placeholders(phids.len()),
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| {
            let policy = CustomPolicy {
                rules: rows::json(row, "rules")?,
                default_action: rows::text(row, "defaultAction")?,
            };
            Ok((Phid::new(rows::text(row, "phid")?), policy))
        })
        .collect()
}

async fn load_spaces(database: &Database) -> Result<HashMap<Phid, Space>, LoaderError> {
    let sql = format!(
        "SELECT phid, viewPolicy, editPolicy, isDefaultNamespace FROM {}",
        database.table("spaces", "spaces_namespace"),
    );
    let rows = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await?;
    let mut spaces = HashMap::new();
    for row in rows {
        let phid = Phid::new(rows::text(&row, "phid")?);
        let subject = PolicySubject::new(Some(phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, rows::text(&row, "viewPolicy")?)
            .with_policy(Capability::EDIT, rows::text(&row, "editPolicy")?);
        spaces.insert(
            phid,
            Space {
                subject,
                is_default: rows::flag(&row, "isDefaultNamespace")?,
            },
        );
    }
    Ok(spaces)
}
