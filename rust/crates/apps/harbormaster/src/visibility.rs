//! Who sees Harbormaster objects: the `willFilterPage()` of each query,
//! which drops the rows whose parent object (loaded with
//! `PhabricatorObjectQuery` or a Harbormaster query) the viewer can't see,
//! then the policy filter on the policies inherited from that parent.
//!
//! The inherited policies are those the parent was just checked against
//! (`getPolicy()` and `hasAutomaticCapability()` delegate to it), without
//! its spaces and extended policies, which only make the parent's own check
//! stricter. So a row passes the policy filter whenever `willFilterPage()`
//! keeps it, Harbormaster being visible to whoever calls its methods; only
//! build plans have policies of their own.

use std::collections::{HashMap, HashSet};

use phab_app_differential::{commit, diff_search, repository, storage as differential};
use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, PhidType};
use phab_db::rows;
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, POLICY_USER, PolicySubject};

use crate::storage::{self, Build, Buildable, Object, Plan, Policies, Step, Target};

/// `PhabricatorObjectQuery` on the objects buildables build: the diffs
/// and commits of `phids` the viewer can see, with their policies. Other
/// objects are left to PHP.
pub(crate) async fn buildable_objects(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<HashMap<Phid, Policies>, MethodError> {
    let mut diffs = Vec::new();
    let mut commits = Vec::new();
    for phid in phids {
        match phid.phid_type() {
            PhidType::DIFF => diffs.push(phid.clone()),
            PhidType::COMMIT => commits.push(phid.clone()),
            _ => return Err(MethodError::Unsupported("buildables of other objects")),
        }
    }
    diffs.sort();
    diffs.dedup();
    commits.sort();
    commits.dedup();
    let context = request.context;
    let database = &context.database;
    let mut objects = HashMap::new();

    let visible: Vec<Phid> = diff_search::visible_diffs(request, &diffs)
        .await?
        .into_iter()
        .map(|(_, phid)| phid)
        .collect();
    if !visible.is_empty() {
        let sql = format!(
            "SELECT phid, revisionID, viewPolicy FROM {} WHERE phid IN ({})",
            database.table("differential", "differential_diff"),
            phab_db::placeholders(visible.len()),
        );
        let most_open = crate::most_open(&context.config);
        for row in phab_db::bind_all(phab_db::sqlx::query(&sql), visible.iter().map(Phid::as_str))
            .fetch_all(database.pool())
            .await?
        {
            // DifferentialDiff::getPolicy(): the diffs of a revision (which
            // the viewer can see, or the diff query drops them) have the
            // most open policy; the others their view policy, for every
            // capability.
            let policy = if rows::opt_int(&row, "revisionID")?.unwrap_or(0) != 0 {
                most_open.to_owned()
            } else {
                rows::text(&row, "viewPolicy")?
            };
            objects.insert(
                Phid::new(rows::text(&row, "phid")?),
                Policies {
                    view: policy.clone(),
                    edit: policy,
                },
            );
        }
    }

    // PhabricatorRepositoryCommit::getPolicy(): the repository's view
    // policy, and anyone can edit.
    for (phid, commit) in commit::load_visible_by_phids(
        database,
        &context.config,
        &request.viewer,
        &commits,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?
    {
        let view = commit
            .repository
            .subject()
            .policy(Capability::VIEW)
            .unwrap_or_default()
            .to_owned();
        objects.insert(
            phid,
            Policies {
                view,
                edit: POLICY_USER.to_owned(),
            },
        );
    }
    Ok(objects)
}

/// `HarbormasterBuildableQuery::willFilterPage()`.
pub(crate) async fn visible_buildables(
    request: &ApiRequest<'_>,
    rows: Vec<Buildable>,
) -> Result<Vec<Buildable>, MethodError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let phids: Vec<Phid> = rows.iter().map(|row| row.object_phid.clone()).collect();
    let objects = buildable_objects(request, &phids).await?;
    Ok(rows
        .into_iter()
        .filter_map(|mut row| {
            row.policies = Some(objects.get(&row.object_phid)?.clone());
            Some(row)
        })
        .collect())
}

/// The policies of the rows of `T` with these PHIDs the viewer can see.
async fn visible_parents<T: Object>(
    request: &ApiRequest<'_>,
    phids: Vec<String>,
    policies: impl Fn(&T) -> Option<(Phid, Policies)>,
) -> Result<HashMap<Phid, Policies>, MethodError> {
    let parents: Vec<T> = storage::load_where(&request.context.database, "phid", &phids).await?;
    Ok(T::visible(request, parents)
        .await?
        .iter()
        .filter_map(policies)
        .collect())
}

/// `HarbormasterBuildQuery::willFilterPage()`: builds of visible
/// buildables.
pub(crate) async fn visible_builds(
    request: &ApiRequest<'_>,
    rows: Vec<Build>,
) -> Result<Vec<Build>, MethodError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let buildables = visible_parents::<Buildable>(
        request,
        storage::phid_strings(rows.iter().map(|row| &row.buildable_phid)),
        |buildable| Some((buildable.phid.clone(), buildable.policies.clone()?)),
    )
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|mut row| {
            row.policies = Some(buildables.get(&row.buildable_phid)?.clone());
            Some(row)
        })
        .collect())
}

/// `HarbormasterBuildTargetQuery::willFilterPage()`: targets of visible
/// builds.
pub(crate) async fn visible_targets(
    request: &ApiRequest<'_>,
    rows: Vec<Target>,
) -> Result<Vec<Target>, MethodError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let builds = visible_parents::<Build>(
        request,
        storage::phid_strings(rows.iter().map(|row| &row.build_phid)),
        |build| Some((build.phid.clone(), build.policies.clone()?)),
    )
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|mut row| {
            row.policies = Some(builds.get(&row.build_phid)?.clone());
            Some(row)
        })
        .collect())
}

/// The `willFilterPage()` of `HarbormasterBuildArtifactQuery` and
/// `HarbormasterBuildLogQuery`: rows of visible targets.
pub(crate) async fn visible_by_target<T>(
    request: &ApiRequest<'_>,
    rows: Vec<T>,
    target: impl Fn(&T) -> &Phid,
    attach: impl Fn(&mut T, Policies),
) -> Result<Vec<T>, MethodError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let targets = visible_parents::<Target>(
        request,
        storage::phid_strings(rows.iter().map(&target)),
        |target| Some((target.phid.clone(), target.policies.clone()?)),
    )
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|mut row| {
            let policies = targets.get(target(&row))?.clone();
            attach(&mut row, policies);
            Some(row)
        })
        .collect())
}

/// `HarbormasterBuildPlanQuery`: the policy filter on the plans' own
/// policies. Plans are subscribable, and nobody is automatically
/// subscribed to them or has automatic capabilities on them.
pub(crate) async fn visible_plans(
    request: &ApiRequest<'_>,
    rows: Vec<Plan>,
) -> Result<Vec<Plan>, MethodError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let context = request.context;
    let most_open = crate::most_open(&context.config);
    let subjects: Vec<PolicySubject> = rows
        .iter()
        .map(|plan| {
            let policies = plan.policies(most_open);
            let mut subject = PolicySubject::new(Some(plan.phid.clone()), ObjectKind::Other)
                .with_policy(Capability::VIEW, policies.view)
                .with_policy(Capability::EDIT, policies.edit);
            subject.subscribable = true;
            subject
        })
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts = phab_policy::loader::load_facts(
        &context.database,
        &context.config,
        &request.viewer,
        &refs,
        &NoExtendedObjects,
        request.now,
    )
    .await?;
    let passed = phab_policy::filter(&request.viewer, &refs, &[Capability::VIEW], &facts)?;
    Ok(rows
        .into_iter()
        .zip(passed)
        .filter_map(|(plan, visible)| visible.then_some(plan))
        .collect())
}

/// `HarbormasterBuildStepQuery::willFilterPage()`: steps of visible
/// plans, which have their policies.
pub(crate) async fn visible_steps(
    request: &ApiRequest<'_>,
    rows: Vec<Step>,
) -> Result<Vec<Step>, MethodError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let most_open = crate::most_open(&request.context.config);
    let plans = visible_parents::<Plan>(
        request,
        storage::phid_strings(rows.iter().map(|row| &row.plan_phid)),
        |plan| Some((plan.phid.clone(), plan.policies(most_open))),
    )
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|mut row| {
            row.policies = Some(plans.get(&row.plan_phid)?.clone());
            Some(row)
        })
        .collect())
}

/// The plans with these PHIDs the viewer can see (the
/// `PhabricatorObjectQuery` of `HarbormasterBuildQuery::didFilterPage()`).
pub(crate) async fn visible_plans_by_phid(
    request: &ApiRequest<'_>,
    phids: Vec<String>,
) -> Result<HashMap<Phid, Plan>, MethodError> {
    let plans: Vec<Plan> = storage::load_where(&request.context.database, "phid", &phids).await?;
    Ok(visible_plans(request, plans)
        .await?
        .into_iter()
        .map(|plan| (plan.phid.clone(), plan))
        .collect())
}

/// `PhabricatorObjectQuery::withNames()` for the search fields which take
/// object names: the PHIDs of the diffs, commits, revisions and
/// repositories the viewer can see. Monograms and other objects are left
/// to PHP.
pub(crate) async fn visible_named_objects(
    request: &ApiRequest<'_>,
    names: &[String],
) -> Result<Vec<String>, MethodError> {
    let mut by_type: HashMap<PhidType, Vec<Phid>> = HashMap::new();
    for name in names {
        if !name.starts_with("PHID-") {
            return Err(MethodError::Unsupported("objects named by monograms"));
        }
        let phid = Phid::new(name.clone());
        match phid.phid_type() {
            PhidType::DIFF | PhidType::COMMIT | PhidType::REVISION | PhidType::REPOSITORY => {
                by_type.entry(phid.phid_type()).or_default().push(phid);
            }
            _ => return Err(MethodError::Unsupported("other objects named")),
        }
    }
    let of_type = |phid_type| by_type.get(&phid_type).cloned().unwrap_or_default();
    let context = request.context;
    let database = &context.database;
    let mut visible: HashSet<Phid> = HashSet::new();
    let mut buildable_phids = of_type(PhidType::DIFF);
    buildable_phids.extend(of_type(PhidType::COMMIT));
    visible.extend(
        buildable_objects(request, &buildable_phids)
            .await?
            .into_keys(),
    );
    let revisions = differential::load_revisions(database, &of_type(PhidType::REVISION)).await?;
    visible.extend(
        differential::visible_revisions(
            database,
            &context.config,
            &request.viewer,
            revisions,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?
        .into_iter()
        .map(|revision| revision.phid),
    );
    let repositories = repository::load(database, &of_type(PhidType::REPOSITORY)).await?;
    visible.extend(
        repository::visible(
            database,
            &context.config,
            &request.viewer,
            repositories,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?
        .into_iter()
        .map(|repository| repository.phid),
    );
    let mut phids: Vec<String> = visible
        .into_iter()
        .map(|phid| phid.as_str().to_owned())
        .collect();
    phids.sort();
    Ok(phids)
}
