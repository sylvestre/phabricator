//! `projects.add`, `projects.remove` and `projects.set`: the `core:edge`
//! transaction of the "has project" edge.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, php};
use phab_db::edges;
use serde_json::json;

use super::lists::{self, Wanted};
use super::{Facts, Plan};
use crate::storage::Revision;

/// The planned transaction: its PHID, the tags it removes and adds.
pub(super) struct Planned {
    pub(super) phid: String,
    pub(super) removed: Vec<String>,
    pub(super) added: Vec<String>,
}

/// The new tags, which need `CAN_EDIT`. Projects the actor can't see,
/// subprojects and milestones (`applyProjectConflictRules()`) and projects
/// with watchers are left to PHP. Updates the projects of `facts`.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &Revision,
    facts: &mut Facts,
) -> Result<Planned, MethodError> {
    let old = facts.projects.clone();
    let new = lists::new_list(&old, wanted);
    let (added, removed) = (lists::minus(&new, &old), lists::minus(&old, &new));
    if added.is_empty() && removed.is_empty() {
        return Err(MethodError::Unsupported("edits without effect"));
    }
    let can_edit = super::can_edit(request, revision).await?;
    if !can_edit {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    if !added.is_empty() {
        let refs: Vec<&str> = added.iter().map(String::as_str).collect();
        if !lists::all_valid(request, &refs).await? {
            return Err(MethodError::Unsupported("tags PHP refuses"));
        }
        check_added(request, &added).await?;
    }
    let mut after: Vec<String> = added.iter().rev().cloned().collect();
    after.extend(lists::minus(&old, &removed));
    facts.projects = after;
    Ok(Planned {
        phid: super::transaction_phid(),
        removed,
        added,
    })
}

/// New tags whose effects are left to PHP: subprojects and milestones
/// (`applyProjectConflictRules()`) and projects with watchers, who get mail.
pub(super) async fn check_added(
    request: &ApiRequest<'_>,
    added: &[String],
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let refs: Vec<&str> = added.iter().map(String::as_str).collect();
    {
        let sql = format!(
            "SELECT 1 FROM {} WHERE phid IN ({}) AND (parentProjectPHID IS NOT NULL OR \
             milestoneNumber IS NOT NULL) LIMIT 1",
            database.table("project", "project"),
            phab_db::placeholders(added.len())
        );
        let nested = phab_db::bind_all(phab_db::sqlx::query(&sql), refs.iter().copied())
            .fetch_optional(database.pool())
            .await?;
        if nested.is_some() {
            return Err(MethodError::Unsupported("subprojects and milestones"));
        }
        let phids: Vec<Phid> = refs.iter().map(|phid| Phid::new(*phid)).collect();
        if !edges::load_edges(database, &phids, &[super::PROJECT_HAS_WATCHER], None)
            .await?
            .is_empty()
        {
            return Err(MethodError::Unsupported("projects with watchers"));
        }
    }
    Ok(())
}

/// The transaction, in the slim format of edges (the removed and the
/// added PHIDs), and the edges with their inverses.
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let (old, new) = (
        php::json_encode(&json!(planned.removed)),
        php::json_encode(&json!(planned.added)),
    );
    let metadata = super::edge_metadata(plan, edges::OBJECT_HAS_PROJECT);
    let row = (planned.phid.as_str(), "core:edge", &*old, &*new, &*metadata);
    super::insert_transaction(request, plan, connection, row).await?;
    write_edges(request, plan, planned, connection).await
}

/// The edges of the planned tags, with their inverses.
pub(super) async fn write_edges(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let revision = plan.revision.phid.as_str();
    for project in &planned.removed {
        super::delete_edge(
            database,
            connection,
            (revision, edges::OBJECT_HAS_PROJECT, project),
        )
        .await?;
    }
    for (seq, project) in (0_i64..).zip(&planned.added) {
        let edge = (revision, edges::OBJECT_HAS_PROJECT, project.as_str());
        super::insert_edge(database, connection, edge, seq, request.now).await?;
    }
    Ok(())
}
