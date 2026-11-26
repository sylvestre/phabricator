//! The rows the engine reads, as Lisk loads them for the omnipotent viewer
//! (`HarbormasterBuildQuery` and the queries it attaches objects with).

use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use serde_json::Value;

use super::{EngineError, unsupported};
use crate::storage::{Object, Plan};

/// An ID from task data, and how `%s` prints it in the failure message.
/// `%Ld` casts what isn't an integer to one, which matches nothing.
pub(super) fn id_param(value: Option<&Value>) -> Result<(i64, String), EngineError> {
    match value {
        Some(Value::Number(number)) if number.is_i64() => {
            let id = number.as_i64().unwrap_or_default();
            Ok((id, id.to_string()))
        }
        Some(Value::String(text)) => {
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
                let id = text
                    .parse()
                    .map_err(|_| unsupported("IDs which overflow integers"))?;
                Ok((id, text.clone()))
            } else {
                Err(unsupported("IDs which are not integers"))
            }
        }
        Some(Value::Bool(true)) => Ok((1, "1".to_owned())),
        _ => Err(unsupported("IDs which are not integers")),
    }
}

/// Whether Lisk writes a JSON column back as it is stored: it decodes it
/// with `json_decode($value, true)` and encodes it again on save, which
/// turns `{}` into `[]` and prints floats its own way.
pub(super) fn json_is_stable(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    lisk_json(&value).is_some_and(|encoded| encoded == raw)
}

/// What Lisk stores for `value` once decoded as PHP decodes it: `None`
/// when it holds floats, which PHP prints differently.
pub(super) fn lisk_json(value: &Value) -> Option<String> {
    if has_floats(value) {
        return None;
    }
    Some(phab_core::php::json_encode(&phab_core::php::round_trip(
        value,
    )))
}

fn has_floats(value: &Value) -> bool {
    match value {
        Value::Number(number) => !number.is_i64() && !number.is_u64(),
        Value::Array(items) => items.iter().any(has_floats),
        Value::Object(map) => map.values().any(has_floats),
        _ => false,
    }
}

/// A `harbormaster_build` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BuildRow {
    pub(super) id: i64,
    pub(super) phid: String,
    pub(super) buildable_phid: String,
    pub(super) plan_phid: String,
    pub(super) status: String,
    pub(super) generation: i64,
    pub(super) parameters: String,
    pub(super) initiator: Option<String>,
    pub(super) plan_auto_key: Option<String>,
}

impl BuildRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: rows::text(row, "phid")?,
            buildable_phid: rows::text(row, "buildablePHID")?,
            plan_phid: rows::text(row, "buildPlanPHID")?,
            status: rows::text(row, "buildStatus")?,
            generation: rows::int(row, "buildGeneration")?,
            parameters: rows::text(row, "buildParameters")?,
            initiator: rows::opt_text(row, "initiatorPHID")?,
            plan_auto_key: rows::opt_text(row, "planAutoKey")?,
        })
    }
}

/// A `harbormaster_buildable` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BuildableRow {
    pub(super) id: i64,
    pub(super) phid: String,
    pub(super) object_phid: String,
    pub(super) container_phid: Option<String>,
    pub(super) status: String,
    pub(super) is_manual: bool,
}

impl BuildableRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: rows::text(row, "phid")?,
            object_phid: rows::text(row, "buildablePHID")?,
            container_phid: rows::opt_text(row, "containerPHID")?,
            status: rows::text(row, "buildableStatus")?,
            is_manual: rows::flag(row, "isManualBuildable")?,
        })
    }
}

/// The diff a buildable builds, with its revision.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DiffRow {
    pub(super) id: i64,
    pub(super) phid: String,
    /// The revision, which `DifferentialDiffQuery` attaches.
    pub(super) revision: Option<RevisionRow>,
}

/// The revision of a diff.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RevisionRow {
    pub(super) id: i64,
    pub(super) phid: String,
    /// The repository, if it is attached (it exists).
    pub(super) repository_phid: Option<String>,
    /// `properties`, decoded.
    pub(super) properties: Value,
    /// `activeDiffPHID`.
    pub(super) active_diff_phid: Option<String>,
}

/// A `harbormaster_buildstep` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StepRow {
    pub(super) phid: String,
    pub(super) class_name: String,
    pub(super) name: Option<String>,
    pub(super) details: Value,
}

impl StepRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        let raw = rows::text(row, "details")?;
        Ok(Self {
            phid: rows::text(row, "phid")?,
            class_name: rows::text(row, "className")?,
            name: rows::opt_text(row, "name")?,
            details: serde_json::from_str(&raw).unwrap_or(Value::Null),
        })
    }
}

/// A `harbormaster_buildtarget` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TargetRow {
    pub(super) id: i64,
    pub(super) phid: String,
    pub(super) build_phid: String,
    pub(super) step_phid: String,
    pub(super) class_name: String,
    pub(super) details: String,
    pub(super) variables: String,
    pub(super) status: String,
    pub(super) started: Option<i64>,
    pub(super) completed: Option<i64>,
    pub(super) generation: i64,
}

impl TargetRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: rows::text(row, "phid")?,
            build_phid: rows::text(row, "buildPHID")?,
            step_phid: rows::text(row, "buildStepPHID")?,
            class_name: rows::text(row, "className")?,
            details: rows::text(row, "details")?,
            variables: rows::text(row, "variables")?,
            status: rows::text(row, "targetStatus")?,
            started: rows::opt_int(row, "dateStarted")?,
            completed: rows::opt_int(row, "dateCompleted")?,
            generation: rows::int(row, "buildGeneration")?,
        })
    }
}

/// A `harbormaster_buildartifact` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ArtifactRow {
    pub(super) id: i64,
    pub(super) target_phid: String,
    pub(super) kind: String,
    pub(super) data: String,
    pub(super) released: bool,
}

impl ArtifactRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            target_phid: rows::text(row, "buildTargetPHID")?,
            kind: rows::text(row, "artifactType")?,
            data: rows::text(row, "artifactData")?,
            released: rows::flag(row, "isReleased")?,
        })
    }
}

/// A `harbormaster_buildmessage` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MessageRow {
    pub(super) id: i64,
    pub(super) author: Option<String>,
    pub(super) receiver: String,
    pub(super) kind: String,
    pub(super) consumed: bool,
}

impl MessageRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            author: rows::opt_text(row, "authorPHID")?,
            receiver: rows::text(row, "receiverPHID")?,
            kind: rows::text(row, "type")?,
            consumed: rows::flag(row, "isConsumed")?,
        })
    }
}

/// The rows of `table` (in the `harbormaster` database) whose `column` is
/// one of `values`, in the order `tail` gives.
async fn load_in<T>(
    database: &Database,
    (table, column): (&str, &str),
    values: &[String],
    tail: &str,
    read: fn(&MySqlRow) -> Result<T, DbError>,
) -> Result<Vec<T>, EngineError> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT * FROM {} WHERE {column} IN ({}) {tail}",
        database.table("harbormaster", table),
        phab_db::placeholders(values.len()),
    );
    let fetched = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        values.iter().map(String::as_str),
    )
    .fetch_all(database.pool())
    .await?;
    Ok(fetched.iter().map(read).collect::<Result<_, _>>()?)
}

/// The build with this ID.
pub(super) async fn build_by_id(
    database: &Database,
    id: i64,
) -> Result<Option<BuildRow>, EngineError> {
    let row = phab_db::sqlx::query(&format!(
        "SELECT * FROM {} WHERE id = ?",
        database.table("harbormaster", "harbormaster_build")
    ))
    .bind(id)
    .fetch_optional(database.pool())
    .await?;
    Ok(row.as_ref().map(BuildRow::from_row).transpose()?)
}

/// The builds of a buildable, newest first.
pub(super) async fn builds_of(
    database: &Database,
    buildable_phid: &str,
) -> Result<Vec<BuildRow>, EngineError> {
    load_in(
        database,
        ("harbormaster_build", "buildablePHID"),
        &[buildable_phid.to_owned()],
        "ORDER BY id DESC",
        BuildRow::from_row,
    )
    .await
}

/// The builds with these PHIDs.
pub(super) async fn builds_by_phid(
    database: &Database,
    phids: &[String],
) -> Result<Vec<BuildRow>, EngineError> {
    load_in(
        database,
        ("harbormaster_build", "phid"),
        phids,
        "ORDER BY id DESC",
        BuildRow::from_row,
    )
    .await
}

/// The buildable with this PHID.
pub(super) async fn buildable_by_phid(
    database: &Database,
    phid: &str,
) -> Result<Option<BuildableRow>, EngineError> {
    Ok(load_in(
        database,
        ("harbormaster_buildable", "phid"),
        &[phid.to_owned()],
        "",
        BuildableRow::from_row,
    )
    .await?
    .into_iter()
    .next())
}

/// The object a buildable builds, as `PhabricatorObjectQuery` loads it for
/// the omnipotent viewer: `None` when it doesn't load, which drops the
/// buildable from its query. Only diffs are ported.
pub(super) async fn buildable_object(
    database: &Database,
    object_phid: &str,
) -> Result<Option<DiffRow>, EngineError> {
    if phab_core::phid::phid_type_of(object_phid) != phab_core::PhidType::DIFF {
        return Err(unsupported("buildables of objects other than diffs"));
    }
    let Some(diff) = phab_db::sqlx::query(&format!(
        "SELECT id, phid, revisionID FROM {} WHERE phid = ?",
        database.table("differential", "differential_diff")
    ))
    .bind(object_phid)
    .fetch_optional(database.pool())
    .await?
    else {
        return Ok(None);
    };
    let id = rows::int(&diff, "id")?;
    let phid = rows::text(&diff, "phid")?;
    let revision = match rows::opt_int(&diff, "revisionID")? {
        // `if (!$diff->getRevisionID())`.
        None | Some(0) => None,
        Some(revision_id) => {
            // `DifferentialDiffQuery::willFilterPage()` drops diffs whose
            // revision is gone.
            let Some(revision) = load_revision(database, revision_id).await? else {
                return Ok(None);
            };
            Some(revision)
        }
    };
    Ok(Some(DiffRow { id, phid, revision }))
}

/// A revision, as `DifferentialRevisionQuery` loads it for the omnipotent
/// viewer: with its repository attached if it still exists.
async fn load_revision(database: &Database, id: i64) -> Result<Option<RevisionRow>, EngineError> {
    let Some(row) = phab_db::sqlx::query(&format!(
        "SELECT id, phid, repositoryPHID, properties, activeDiffPHID FROM {} WHERE id = ?",
        database.table("differential", "differential_revision")
    ))
    .bind(id)
    .fetch_optional(database.pool())
    .await?
    else {
        return Ok(None);
    };
    let mut repository_phid =
        rows::opt_text(&row, "repositoryPHID")?.filter(|phid| !phid.is_empty());
    if let Some(repository) = &repository_phid {
        let exists = phab_db::sqlx::query(&format!(
            "SELECT 1 FROM {} WHERE phid = ?",
            database.table("repository", "repository")
        ))
        .bind(repository)
        .fetch_optional(database.pool())
        .await?;
        // The omnipotent viewer loads the revision anyway, without it.
        if exists.is_none() {
            repository_phid = None;
        }
    }
    let properties = rows::text(&row, "properties")?;
    Ok(Some(RevisionRow {
        id: rows::int(&row, "id")?,
        phid: rows::text(&row, "phid")?,
        repository_phid,
        properties: serde_json::from_str(&properties).unwrap_or(Value::Null),
        active_diff_phid: rows::opt_text(&row, "activeDiffPHID")?,
    }))
}

/// `DifferentialDiff::getHarbormasterContainerPHID()`: the PHID of the
/// diff's revision, loaded by ID without a query.
pub(super) async fn diff_container(
    database: &Database,
    object_phid: &str,
) -> Result<Option<String>, EngineError> {
    let row = phab_db::sqlx::query(&format!(
        "SELECT revision.phid FROM {} diff JOIN {} revision ON revision.id = diff.revisionID \
         WHERE diff.phid = ?",
        database.table("differential", "differential_diff"),
        database.table("differential", "differential_revision"),
    ))
    .bind(object_phid)
    .fetch_optional(database.pool())
    .await?;
    Ok(row
        .as_ref()
        .map(|row| rows::text(row, "phid"))
        .transpose()?)
}

/// The plans with these PHIDs.
pub(super) async fn plans(database: &Database, phids: &[String]) -> Result<Vec<Plan>, EngineError> {
    load_in(
        database,
        ("harbormaster_buildplan", "phid"),
        phids,
        "",
        <Plan as Object>::from_row,
    )
    .await
}

/// The steps of a plan, newest first.
pub(super) async fn steps_of(
    database: &Database,
    plan_phid: &str,
) -> Result<Vec<StepRow>, EngineError> {
    load_in(
        database,
        ("harbormaster_buildstep", "buildPlanPHID"),
        &[plan_phid.to_owned()],
        "ORDER BY id DESC",
        StepRow::from_row,
    )
    .await
}

/// The targets of a build, every generation, newest first.
pub(super) async fn targets_of(
    database: &Database,
    build_phid: &str,
) -> Result<Vec<TargetRow>, EngineError> {
    load_in(
        database,
        ("harbormaster_buildtarget", "buildPHID"),
        &[build_phid.to_owned()],
        "ORDER BY id DESC",
        TargetRow::from_row,
    )
    .await
}

/// The target with this ID.
pub(super) async fn target_by_id(
    database: &Database,
    id: i64,
) -> Result<Option<TargetRow>, EngineError> {
    let row = phab_db::sqlx::query(&format!(
        "SELECT * FROM {} WHERE id = ?",
        database.table("harbormaster", "harbormaster_buildtarget")
    ))
    .bind(id)
    .fetch_optional(database.pool())
    .await?;
    Ok(row.as_ref().map(TargetRow::from_row).transpose()?)
}

/// The artifacts of these targets, newest first.
pub(super) async fn artifacts_of(
    database: &Database,
    target_phids: &[String],
) -> Result<Vec<ArtifactRow>, EngineError> {
    load_in(
        database,
        ("harbormaster_buildartifact", "buildTargetPHID"),
        target_phids,
        "ORDER BY id DESC",
        ArtifactRow::from_row,
    )
    .await
}

/// The unconsumed messages to these receivers, oldest first.
pub(super) async fn unconsumed_messages(
    database: &Database,
    receivers: &[String],
) -> Result<Vec<MessageRow>, EngineError> {
    load_in(
        database,
        ("harbormaster_buildmessage", "receiverPHID"),
        receivers,
        "AND isConsumed = 0 ORDER BY id ASC",
        MessageRow::from_row,
    )
    .await
}

/// `HeraldTranscript::loadXHeraldRulesHeader()`.
pub(super) async fn saved_header(
    database: &Database,
    phid: &str,
) -> Result<Option<String>, EngineError> {
    let row = phab_db::sqlx::query(&format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    ))
    .bind(phid)
    .fetch_optional(database.pool())
    .await?;
    Ok(row
        .as_ref()
        .map(|row| rows::opt_text(row, "header"))
        .transpose()?
        .flatten())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_ids_like_qsprintf() {
        assert_eq!(id_param(Some(&json!(5))).unwrap(), (5, "5".to_owned()));
        assert_eq!(id_param(Some(&json!("12"))).unwrap(), (12, "12".to_owned()));
        assert!(id_param(Some(&json!("x"))).is_err());
        assert!(id_param(Some(&json!(1.5))).is_err());
    }

    #[test]
    fn knows_which_json_lisk_keeps() {
        assert!(json_is_stable("[]"));
        assert!(json_is_stable(r#"{"uri":"https:\/\/a.test\/"}"#));
        assert!(!json_is_stable("{}"));
        assert!(!json_is_stable(r#"{"uri":"https://a.test/"}"#));
        assert!(!json_is_stable(r#"{"0":"a"}"#));
        assert!(!json_is_stable(r#"{"d":1.5}"#));
        assert!(!json_is_stable("nope"));
        assert_eq!(lisk_json(&json!({})).as_deref(), Some("[]"));
    }
}
