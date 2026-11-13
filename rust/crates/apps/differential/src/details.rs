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
