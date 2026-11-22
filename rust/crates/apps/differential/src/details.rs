//! What the revision page shows beyond [`crate::Revision`]: the text
//! fields, dates, active diff, Mozilla's Bugzilla bug and the stack.

use phab_core::Phid;
use phab_db::{Database, DbError, edges, rows};

/// The custom field key of `DifferentialBugzillaBugIDField`
/// (moz-extensions).
const BUGZILLA_BUG_FIELD: &str = "differential:bugzilla-bug-id";

/// The text and dates of a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionDetails {
    /// The summary, in Remarkup.
    pub summary: String,
    /// The test plan, in Remarkup.
    pub test_plan: String,
    /// When the revision was created.
    pub date_created: i64,
    /// When it last changed.
    pub date_modified: i64,
    /// The diff the revision shows.
    pub active_diff_phid: Option<Phid>,
}

/// Loads the details of a revision.
pub async fn load_details(
    database: &Database,
    revision: &Phid,
) -> Result<Option<RevisionDetails>, DbError> {
    let sql = format!(
        "SELECT summary, testPlan, dateCreated, dateModified, activeDiffPHID FROM {} WHERE phid = ?",
        database.table("differential", "differential_revision")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(revision.as_str())
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| {
        Ok(RevisionDetails {
            summary: rows::text(&row, "summary")?,
            test_plan: rows::text(&row, "testPlan")?,
            date_created: rows::int(&row, "dateCreated")?,
            date_modified: rows::int(&row, "dateModified")?,
            active_diff_phid: rows::opt_phid(&row, "activeDiffPHID")?,
        })
    })
    .transpose()
}

/// `DifferentialBugzillaBugIDField`: the bug number, if set. PHP casts the
/// stored value with `(int)`, so anything else shows as not provided.
pub async fn load_bug_id(database: &Database, revision: &Phid) -> Result<Option<i64>, DbError> {
    let sql = format!(
        "SELECT fieldValue FROM {} WHERE objectPHID = ? AND fieldIndex = ?",
        database.table("differential", "differential_customfieldstorage")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(revision.as_str())
        .bind(phab_core::hash::digest_for_index(
            BUGZILLA_BUG_FIELD.as_bytes(),
        ))
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let value = rows::text(&row, "fieldValue")?;
    let digits: String = value
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Ok(digits.parse().ok().filter(|bug: &i64| *bug > 0))
}

/// The revisions this one depends on (its parents in the stack) and the
/// ones depending on it (its children), as PHIDs.
pub async fn load_stack_edges(
    database: &Database,
    revision: &Phid,
) -> Result<(Vec<Phid>, Vec<Phid>), DbError> {
    let revision = std::slice::from_ref(revision);
    let parents = edges::load_destination_phids(
        database,
        revision,
        &[edges::REVISION_DEPENDS_ON_REVISION],
        None,
    )
    .await?;
    let children = edges::load_destination_phids(
        database,
        revision,
        &[edges::REVISION_DEPENDED_ON_BY_REVISION],
        None,
    )
    .await?;
    Ok((parents, children))
}

/// The lint and unit statuses of a diff (`DifferentialLintStatus`,
/// `DifferentialUnitStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffStatus {
    /// The diff's ID.
    pub id: i64,
    /// `lintStatus`.
    pub lint: i64,
    /// `unitStatus`.
    pub unit: i64,
}

/// The lint and unit statuses of the diff with this PHID.
pub async fn load_diff_status(
    database: &Database,
    diff: &Phid,
) -> Result<Option<DiffStatus>, DbError> {
    let sql = format!(
        "SELECT id, lintStatus, unitStatus FROM {} WHERE phid = ?",
        database.table("differential", "differential_diff")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(diff.as_str())
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| {
        Ok(DiffStatus {
            id: rows::int(&row, "id")?,
            lint: rows::int(&row, "lintStatus")?,
            unit: rows::int(&row, "unitStatus")?,
        })
    })
    .transpose()
}

/// Whether a diff has a truthy property, like the legacy `arc:lint`
/// messages (`DifferentialDiffProperty`).
pub async fn has_diff_property(
    database: &Database,
    diff_id: i64,
    name: &str,
) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT data FROM {} WHERE diffID = ? AND name = ?",
        database.table("differential", "differential_diffproperty")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .bind(name)
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    Ok(rows::json(&row, "data").is_ok_and(|data| phab_core::php::is_truthy(&data)))
}

/// `DifferentialRevision::getShouldBroadcast()`: the `draft.broadcast`
/// property, true when missing.
pub async fn should_broadcast(database: &Database, revision: &Phid) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT properties FROM {} WHERE phid = ?",
        database.table("differential", "differential_revision")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(revision.as_str())
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(true);
    };
    Ok(match rows::json(&row, "properties") {
        Ok(serde_json::Value::Object(properties)) => properties
            .get("draft.broadcast")
            .is_none_or(phab_core::php::is_truthy),
        _ => true,
    })
}
