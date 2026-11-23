//! `differential.createinline` (`DifferentialCreateInlineConduitAPIMethod`):
//! a draft inline comment of the caller on a file of a diff, which the
//! caller's next comment publishes. It saves the comment's row alone
//! (`DifferentialInlineComment::save()`): no transaction, no draft edge.
//!
//! Left to PHP: values PHP casts or compares loosely (IDs which are not
//! integers, numeric paths, `isNewFile` other than a boolean, `0` or `1`),
//! values the columns refuse, paths several files of the diff have,
//! revisions the caller can't see (a policy exception), diffs without a
//! revision and revisions without a diff (PHP fails).

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php};
use phab_db::rows;
use serde_json::{Value, json};

use crate::diff_editor;

const ERR_BAD_REVISION: &str = "ERR-BAD-REVISION";
const ERR_BAD_DIFF: &str = "ERR-BAD-DIFF";
const ERR_NEED_DIFF: &str = "ERR-NEED-DIFF";
const ERR_NEED_FILE: &str = "ERR-NEED-FILE";
const ERR_BAD_FILE: &str = "ERR-BAD-FILE";

/// `PhabricatorOldWorldContentSource`, which `getTransactionCommentForSave()`
/// gives the comment.
const LEGACY_SOURCE: &str = r#"{"source":"legacy","params":[]}"#;

/// The largest value of the `uint32` line columns.
const MAX_LINE: u64 = 4_294_967_295;

fn conduit_error(code: &str) -> MethodError {
    MethodError::Conduit {
        code: code.to_owned(),
        info: None,
    }
}

/// A revision or diff ID: none when falsy (`if ($rid)`), else an integer
/// or its digits.
fn read_id(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    let unusual = MethodError::Unsupported("IDs which are not integers");
    match value {
        None => Ok(None),
        Some(value) if !php::is_truthy(value) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .filter(|id| *id > 0)
            .map(Some)
            .ok_or(unusual),
        Some(Value::String(text)) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
            text.parse().map(Some).map_err(|_| unusual)
        }
        Some(_) => Err(unusual),
    }
}

/// `(int)` of `isNewFile`, for the values which give `0` or `1`.
fn read_is_new_file(value: Option<&Value>) -> Result<i64, MethodError> {
    let flag = match value {
        None | Some(Value::Null) => Some(0),
        Some(Value::Bool(value)) => Some(i64::from(*value)),
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => {
            // The leading integer, `0` without one.
            let text = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
            let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                Some(0)
            } else {
                digits.parse().ok()
            }
        }
        Some(_) => None,
    };
    flag.filter(|flag| *flag == 0 || *flag == 1)
        .ok_or(MethodError::Unsupported("unusual isNewFile values"))
}

/// A line number or length as the column takes it: an integer or its
/// digits, kept as given for the answer.
fn read_line(value: Option<&Value>) -> Result<(i64, Value), MethodError> {
    let line = match value {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text))
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            text.parse().ok()
        }
        _ => None,
    };
    match line {
        Some(line) if line <= MAX_LINE => Ok((
            i64::try_from(line).map_err(MethodError::internal)?,
            value.cloned().unwrap_or_default(),
        )),
        _ => Err(MethodError::Unsupported("line numbers the columns refuse")),
    }
}

/// A diff: its ID and its revision's.
struct Diff {
    id: i64,
    revision_id: Option<i64>,
}

/// `id(new DifferentialDiff())->load($id)` and `loadActiveDiff()`: no
/// policy is checked.
async fn load_diff(
    request: &ApiRequest<'_>,
    condition: &str,
    id: i64,
) -> Result<Option<Diff>, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id, revisionID FROM {} WHERE {condition}",
        database.table("differential", "differential_diff")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(Diff {
        id: rows::int(&row, "id")?,
        revision_id: rows::opt_int(&row, "revisionID")?,
    }))
}

/// The revision and the diff the comment goes on.
async fn target(request: &ApiRequest<'_>) -> Result<(Phid, i64), MethodError> {
    let database = &request.context.database;
    let config = &request.context.config;
    let revision_id = read_id(request.value("revisionID"))?;
    let diff_id = read_id(request.value("diffID"))?;
    let (revision_id, diff) = match (revision_id, diff_id) {
        (Some(revision_id), diff_id) => {
            let Some(revision) = crate::storage::load_revisions_by_id(database, &[revision_id])
                .await?
                .pop()
            else {
                return Err(conduit_error(ERR_BAD_REVISION));
            };
            let visible = crate::storage::visible_revisions(
                database,
                config,
                &request.viewer,
                vec![revision],
                request.now,
            )
            .await
            .map_err(MethodError::internal)?;
            if visible.is_empty() {
                return Err(MethodError::Unsupported(
                    "revisions the viewer can't see, a policy exception",
                ));
            }
            let diff = match diff_id {
                None => load_diff(
                    request,
                    "revisionID = ? ORDER BY id DESC LIMIT 1",
                    revision_id,
                )
                .await?
                .ok_or(MethodError::Unsupported("revisions without a diff"))?,
                Some(diff_id) => load_diff(request, "id = ?", diff_id)
                    .await?
                    .filter(|diff| diff.revision_id == Some(revision_id))
                    .ok_or_else(|| conduit_error(ERR_BAD_DIFF))?,
            };
            (revision_id, diff)
        }
        (None, Some(diff_id)) => {
            let diff = load_diff(request, "id = ?", diff_id)
                .await?
                .ok_or_else(|| conduit_error(ERR_BAD_DIFF))?;
            let revision_id = diff
                .revision_id
                .filter(|id| *id > 0)
                .ok_or(MethodError::Unsupported("diffs without a revision"))?;
            (revision_id, diff)
        }
        (None, None) => return Err(conduit_error(ERR_NEED_DIFF)),
    };
    // setRevisionID() loads the revision again, without a policy.
    let revision = crate::storage::load_revisions_by_id(database, &[revision_id])
        .await?
        .pop()
        .ok_or(MethodError::Unsupported("diffs of missing revisions"))?;
    Ok((revision.phid, diff.id))
}

/// A changeset of the diff: its ID, file name and old file name.
type Changeset = (i64, String, Option<String>);

/// The changeset of `path` in diff `diff_id`.
async fn find_changeset(
    request: &ApiRequest<'_>,
    diff_id: i64,
    path: &str,
) -> Result<Changeset, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id, filename, oldFile FROM {} WHERE diffID = ?",
        database.table("differential", "differential_changeset")
    );
    let mut found: Vec<Changeset> = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .fetch_all(database.pool())
        .await?
    {
        let filename = rows::text(&row, "filename")?;
        if filename == path {
            found.push((
                rows::int(&row, "id")?,
                filename,
                rows::opt_text(&row, "oldFile")?,
            ));
        }
    }
    match found.len() {
        0 => Err(conduit_error(ERR_BAD_FILE)),
        // PHP takes the last in an order MySQL picks.
        1 => Ok(found.remove(0)),
        _ => Err(MethodError::Unsupported("paths of several files of a diff")),
    }
}

/// `differential.createinline`.
pub struct CreateInline;

impl CreateInline {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let actor = diff_editor::actor_phid(request)?;
        let (revision_phid, diff_id) = target(request).await?;
        let path = match request.value("filePath") {
            Some(value) if !php::is_truthy(value) => return Err(conduit_error(ERR_NEED_FILE)),
            None => return Err(conduit_error(ERR_NEED_FILE)),
            Some(Value::String(path)) => path.clone(),
            Some(_) => return Err(MethodError::Unsupported("paths which are not strings")),
        };
        // `==` compares numeric strings as numbers.
        if path.trim().parse::<f64>().is_ok() {
            return Err(MethodError::Unsupported("numeric paths"));
        }
        let (changeset_id, filename, old_file) = find_changeset(request, diff_id, &path).await?;
        let Some(Value::String(content)) = request.value("content") else {
            return Err(MethodError::Unsupported("contents which are not strings"));
        };
        let is_new_file = read_is_new_file(request.value("isNewFile"))?;
        let (line_number, line_answer) = read_line(request.value("lineNumber"))?;
        let (line_length, length_answer) = match request.value("lineLength") {
            None => (0, json!(0)),
            value => read_line(value)?,
        };
        let database = &request.context.database;
        let phid = Phid::generate(PhidType::TRANSACTION_COMMENT, None);
        let id = phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
             commentVersion, content, contentSource, isDeleted, dateCreated, dateModified, \
             revisionPHID, changesetID, isNewFile, lineNumber, lineLength, fixedState, \
             hasReplies, replyToCommentPHID, attributes) VALUES (?, NULL, ?, 'public', ?, 1, ?, \
             ?, 0, ?, ?, ?, ?, ?, ?, ?, NULL, 0, NULL, '[]')",
            database.table("differential", "differential_transaction_comment")
        ))
        .bind(phid.as_str())
        .bind(&actor)
        .bind(&actor)
        .bind(content)
        .bind(LEGACY_SOURCE)
        .bind(request.now)
        .bind(request.now)
        .bind(revision_phid.as_str())
        .bind(changeset_id)
        .bind(is_new_file)
        .bind(line_number)
        .bind(line_length)
        .execute(database.pool())
        .await?
        .last_insert_id();
        // buildInlineInfoDictionary(): the values as set, the changeset's
        // as read (strings).
        Ok(json!({
            "id": id,
            "authorPHID": actor,
            "filePath": if is_new_file == 1 { Some(filename) } else { old_file },
            "isNewFile": is_new_file,
            "lineNumber": line_answer,
            "lineLength": length_answer,
            "diffID": diff_id.to_string(),
            "content": content,
        }))
    }
}

impl Method for CreateInline {
    fn name(&self) -> &'static str {
        "differential.createinline"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "revisionID",
            "diffID",
            "filePath",
            "isNewFile",
            "lineNumber",
            "lineLength",
            "content",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[
            (ERR_BAD_REVISION, "Bad revision ID."),
            (
                ERR_BAD_DIFF,
                "Bad diff ID, or diff does not belong to revision.",
            ),
            (
                ERR_NEED_DIFF,
                "Neither revision ID nor diff ID was provided.",
            ),
            (ERR_NEED_FILE, "A file path was not provided."),
            (
                ERR_BAD_FILE,
                "Requested file doesn't exist in this revision.",
            ),
        ]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ids_like_php() {
        assert_eq!(read_id(None), Ok(None));
        assert_eq!(read_id(Some(&json!(0))), Ok(None));
        assert_eq!(read_id(Some(&json!("0"))), Ok(None));
        assert_eq!(read_id(Some(&json!(""))), Ok(None));
        assert_eq!(read_id(Some(&json!(12))), Ok(Some(12)));
        assert_eq!(read_id(Some(&json!("12"))), Ok(Some(12)));
        assert!(read_id(Some(&json!("12a"))).is_err());
        assert!(read_id(Some(&json!([1]))).is_err());
    }

    #[test]
    fn casts_is_new_file_like_php() {
        assert_eq!(read_is_new_file(Some(&json!(true))), Ok(1));
        assert_eq!(read_is_new_file(Some(&json!(false))), Ok(0));
        assert_eq!(read_is_new_file(Some(&json!("1"))), Ok(1));
        assert_eq!(read_is_new_file(Some(&json!("true"))), Ok(0));
        assert_eq!(read_is_new_file(None), Ok(0));
        assert!(read_is_new_file(Some(&json!(2))).is_err());
    }

    #[test]
    fn keeps_lines_as_given() {
        assert_eq!(read_line(Some(&json!(3))), Ok((3, json!(3))));
        assert_eq!(read_line(Some(&json!("3"))), Ok((3, json!("3"))));
        assert!(read_line(Some(&json!(-1))).is_err());
        assert!(read_line(None).is_err());
        assert!(read_line(Some(&Value::Null)).is_err());
    }
}
