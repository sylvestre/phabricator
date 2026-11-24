//! `PhabricatorRepositoryGitCommitChangeParserWorker`
//! (`PhabricatorRepositoryCommitChangeParserWorker::parseCommit()`): the
//! paths a commit changes, from its `--raw` diff
//! (`diffusion.internal.gitrawdiffquery`), written to `repository_path`
//! and `repository_pathchange`, then the `IMPORTED_CHANGE` flag and a
//! search index task.

use indexmap::IndexMap;
use md5::{Digest as _, Md5};
use phab_core::php;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, DbError, rows};
use serde_json::json;

use crate::import::commit::{FollowUp, ParserTask};
use crate::import::{IMPORTED_CHANGE, ImportError, PRIORITY_INDEX, PUBLISHER, SEARCH_WORKER, now};

/// `DifferentialChangeType::TYPE_ADD`.
const TYPE_ADD: i64 = 1;
/// `TYPE_CHANGE`.
const TYPE_CHANGE: i64 = 2;
/// `TYPE_DELETE`.
const TYPE_DELETE: i64 = 3;
/// `TYPE_MOVE_AWAY`.
const TYPE_MOVE_AWAY: i64 = 4;
/// `TYPE_COPY_AWAY`.
const TYPE_COPY_AWAY: i64 = 5;
/// `TYPE_MOVE_HERE`.
const TYPE_MOVE_HERE: i64 = 6;
/// `TYPE_COPY_HERE`.
const TYPE_COPY_HERE: i64 = 7;
/// `TYPE_MULTICOPY`.
const TYPE_MULTICOPY: i64 = 8;
/// `TYPE_CHILD`.
const TYPE_CHILD: i64 = 10;

/// `DifferentialChangeType::FILE_NORMAL`.
const FILE_NORMAL: i64 = 7;
/// `FILE_DIRECTORY`.
const FILE_DIRECTORY: i64 = 4;
/// `FILE_SYMLINK`.
const FILE_SYMLINK: i64 = 5;
/// `FILE_SUBMODULE`.
const FILE_SUBMODULE: i64 = 8;

/// A change of `parseCommitChanges()`, before its paths have IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `path`.
    pub path: String,
    /// `changeType`.
    pub change_type: i64,
    /// `fileType`.
    pub file_type: i64,
    /// `isDirect`.
    pub is_direct: bool,
    /// `targetPath`.
    pub target_path: Option<String>,
    /// `targetCommitID`.
    pub target_commit: bool,
}

/// `intval($mode, 8)`: the octal digits up to the first other character.
fn octal(mode: &str) -> i64 {
    let mode = mode.trim_start();
    let (negative, digits) = match mode.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mode.strip_prefix('+').unwrap_or(mode)),
    };
    let mut value: i64 = 0;
    for digit in digits.chars().map_while(|character| character.to_digit(8)) {
        value = value.saturating_mul(8).saturating_add(i64::from(digit));
    }
    if negative { -value } else { value }
}

/// The changes of a raw diff, keyed by path in PHP's order.
#[allow(clippy::too_many_lines)]
pub fn parse_raw_diff(raw: &str) -> Result<IndexMap<String, Change>, ImportError> {
    let mut changes: IndexMap<String, Change> = IndexMap::new();
    let mut move_away: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut copy_away: IndexMap<String, Vec<String>> = IndexMap::new();
    for line in raw.split('\n') {
        if crate::php::trim(line).is_empty() {
            continue;
        }
        // preg_split('/ +/', $line, 5).
        let fields: Vec<&str> = split_spaces(line, 5);
        if fields.len() < 5 {
            return Err(ImportError::unsupported("an unusual raw diff line"));
        }
        let (old_mode, new_mode, more) = (fields[0], fields[1], fields[4]);
        let mut pieces = more.split('\t');
        let action = pieces.next().unwrap_or_default();
        let src = pieces.next();
        let dst = pieces.next();
        let Some(src) = src else {
            return Err(ImportError::unsupported("a raw diff line without a path"));
        };
        let src_path = format!("/{src}");
        let dst_path = dst
            .filter(|dst| php::is_truthy_str(dst))
            .map(|dst| format!("/{dst}"));
        let _ = octal(old_mode);
        let new_mode = octal(new_mode);
        let file_type = match new_mode & 0o160_000 {
            0o160_000 => FILE_SUBMODULE,
            0o120_000 => FILE_SYMLINK,
            0o040_000 => FILE_DIRECTORY,
            _ => FILE_NORMAL,
        };
        let mut change_path = src_path.clone();
        let mut target = None;
        let mut is_direct = true;
        let change_type = match action.chars().next() {
            Some('A') => TYPE_ADD,
            Some('D') => TYPE_DELETE,
            Some(kind @ ('C' | 'R')) => {
                let Some(dst_path) = dst_path else {
                    return Err(ImportError::unsupported("a copy without a destination"));
                };
                change_path.clone_from(&dst_path);
                target = Some(src_path.clone());
                let away = if kind == 'C' {
                    &mut copy_away
                } else {
                    &mut move_away
                };
                away.entry(src_path.clone()).or_default().push(dst_path);
                if kind == 'C' {
                    TYPE_COPY_HERE
                } else {
                    TYPE_MOVE_HERE
                }
            }
            Some('T' | 'M') => {
                if file_type == FILE_DIRECTORY {
                    is_direct = false;
                    TYPE_CHILD
                } else {
                    TYPE_CHANGE
                }
            }
            _ => {
                return Err(ImportError::Failed(format!(
                    "Failed to parse line '{line}'."
                )));
            }
        };
        let target_commit = target.is_some();
        changes.insert(
            change_path.clone(),
            Change {
                path: change_path,
                change_type,
                file_type,
                is_direct,
                target_path: target,
                target_commit,
            },
        );
    }
    changes.insert(
        "/".to_owned(),
        Change {
            path: "/".to_owned(),
            change_type: TYPE_CHILD,
            file_type: FILE_DIRECTORY,
            is_direct: false,
            target_path: None,
            target_commit: false,
        },
    );
    for (path, destinations) in &copy_away {
        let (change_type, is_direct) = if move_away.shift_remove(path).is_some() {
            (TYPE_MULTICOPY, true)
        } else {
            // Direct if the source was also changed.
            (TYPE_COPY_AWAY, changes.contains_key(path))
        };
        let file_type = changes[&destinations[0]].file_type;
        changes.insert(
            path.clone(),
            Change {
                path: path.clone(),
                change_type,
                file_type,
                is_direct,
                target_path: None,
                target_commit: false,
            },
        );
    }
    for (path, destinations) in &move_away {
        let file_type = changes[&destinations[0]].file_type;
        changes.insert(
            path.clone(),
            Change {
                path: path.clone(),
                change_type: TYPE_MOVE_AWAY,
                file_type,
                is_direct: true,
                target_path: None,
                target_commit: false,
            },
        );
    }
    Ok(changes)
}

/// `preg_split('/ +/', $line, $limit)`.
fn split_spaces(line: &str, limit: usize) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut rest = line;
    while fields.len() + 1 < limit {
        let Some(start) = rest.find(' ') else {
            break;
        };
        let end = start
            + rest[start..]
                .bytes()
                .take_while(|byte| *byte == b' ')
                .count();
        fields.push(&rest[..start]);
        rest = &rest[end..];
    }
    fields.push(rest);
    fields
}

/// `md5()`, in hex.
pub(crate) fn md5_hex(text: &str) -> String {
    hex::encode(Md5::digest(text.as_bytes()))
}

/// `lookupPaths()`.
async fn lookup_paths(
    database: &Database,
    paths: &[String],
) -> Result<IndexMap<String, i64>, DbError> {
    let mut map = IndexMap::new();
    for chunk in paths.chunks(128) {
        let hashes: Vec<String> = chunk.iter().map(|path| md5_hex(path)).collect();
        let sql = format!(
            "SELECT path, id FROM {} WHERE pathHash IN ({})",
            database.table("repository", "repository_path"),
            phab_db::placeholders(hashes.len())
        );
        for row in phab_db::bind_all(sqlx::query(&sql), hashes)
            .fetch_all(database.pool())
            .await?
        {
            map.insert(rows::text(&row, "path")?, rows::int(&row, "id")?);
        }
    }
    Ok(map)
}

/// `lookupOrCreatePaths()`.
pub async fn lookup_or_create_paths(
    database: &Database,
    paths: &[String],
) -> Result<IndexMap<String, i64>, DbError> {
    let mut map = lookup_paths(database, paths).await?;
    let missing: Vec<String> = paths
        .iter()
        .filter(|path| !map.contains_key(*path))
        .cloned()
        .collect();
    if !missing.is_empty() {
        for chunk in missing.chunks(128) {
            let sql = format!(
                "INSERT IGNORE INTO {} (path, pathHash) VALUES {}",
                database.table("repository", "repository_path"),
                vec!["(?, ?)"; chunk.len()].join(", ")
            );
            let mut query = sqlx::query(&sql);
            for path in chunk {
                query = query.bind(path).bind(md5_hex(path));
            }
            query.execute(database.pool()).await?;
        }
        for (path, id) in lookup_paths(database, &missing).await? {
            map.entry(path).or_insert(id);
        }
    }
    Ok(map)
}

/// Checks what the step needs before it writes anything: the commit hint
/// and the raw diff. `None` when the commit is unreadable (nothing is
/// done) or the step is skipped.
pub async fn prepare(database: &Database, task: &ParserTask) -> Result<Prepared, ImportError> {
    let hint: Option<String> = sqlx::query_scalar(&format!(
        "SELECT hintType FROM {} WHERE repositoryPHID = ? AND oldCommitIdentifier = ?",
        database.table("repository", "repository_commithint")
    ))
    .bind(&task.repository.phid)
    .bind(&task.commit.identifier)
    .fetch_optional(database.pool())
    .await?;
    if hint.as_deref() == Some("unreadable") {
        return Ok(Prepared::Unreadable);
    }
    if task.should_skip_step(IMPORTED_CHANGE) {
        return Ok(Prepared::Skip);
    }
    let copy = task.repository.working_copy()?;
    let raw = crate::internal_git_raw_diff_query::raw_diff(&copy, &task.commit.identifier).await?;
    Ok(Prepared::Parse(parse_raw_diff(&raw)?))
}

/// What [`prepare`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// The commit hint says the commit is unreadable.
    Unreadable,
    /// The step was done before.
    Skip,
    /// The changes to write.
    Parse(IndexMap<String, Change>),
}

/// `parseCommit()` with what [`prepare`] found; returns the next step.
pub async fn parse(
    database: &Database,
    task: &mut ParserTask,
    prepared: Prepared,
) -> Result<Option<FollowUp>, ImportError> {
    match prepared {
        Prepared::Unreadable => return Ok(None),
        Prepared::Skip => {}
        Prepared::Parse(changes) => {
            write_changes(database, task, &changes).await?;
            task.commit
                .write_import_status_flag(database, IMPORTED_CHANGE)
                .await?;
            // PhabricatorSearchWorker::queueDocumentForIndexing(), at once.
            let data = php::json_encode(&json!({
                "documentPHID": task.commit.phid,
                "parameters": [],
                "strict": false,
            }));
            let mut connection = database.pool().acquire().await?;
            schedule_task(
                &mut connection,
                database,
                Task {
                    class: SEARCH_WORKER,
                    data: &data,
                    priority: PRIORITY_INDEX,
                    object_phid: Some(&task.commit.phid),
                },
                now(),
            )
            .await?;
        }
    }
    Ok(task.followup(PUBLISHER))
}

/// The paths, then `writeCommitChanges()`.
async fn write_changes(
    database: &Database,
    task: &ParserTask,
    changes: &IndexMap<String, Change>,
) -> Result<(), ImportError> {
    let mut paths: IndexMap<String, ()> = IndexMap::new();
    for change in changes.values() {
        paths.insert(change.path.clone(), ());
        if let Some(target) = change
            .target_path
            .as_ref()
            .filter(|target| php::is_truthy_str(target))
        {
            paths.insert(target.clone(), ());
        }
    }
    let paths: Vec<String> = paths.into_keys().collect();
    let ids = lookup_or_create_paths(database, &paths).await?;
    let path_id = |path: &str| {
        ids.get(path)
            .copied()
            .ok_or_else(|| ImportError::Failed(format!("No ID for path \"{path}\".")))
    };
    let table = database.table("repository", "repository_pathchange");
    let commit = &task.commit;
    sqlx::query(&format!("DELETE FROM {table} WHERE commitID = ?"))
        .bind(commit.id)
        .execute(database.pool())
        .await?;
    let mut values = Vec::new();
    for change in changes.values() {
        let target_path = match change.target_path.as_deref() {
            Some(target) if php::is_truthy_str(target) => Some(path_id(target)?),
            _ => None,
        };
        values.push((
            path_id(&change.path)?,
            // nonempty((int)...): 0 is NULL.
            target_path.filter(|id| *id != 0),
            change
                .target_commit
                .then_some(commit.id)
                .filter(|id| *id != 0),
            change.change_type,
            change.file_type,
            i64::from(change.is_direct),
        ));
    }
    for chunk in values.chunks(512) {
        let sql = format!(
            "INSERT INTO {table} (repositoryID, pathID, commitID, targetPathID, targetCommitID, \
             changeType, fileType, isDirect, commitSequence) VALUES {}",
            vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query(&sql);
        for (path, target_path, target_commit, change_type, file_type, is_direct) in chunk {
            query = query
                .bind(task.repository.id)
                .bind(path)
                .bind(commit.id)
                .bind(target_path)
                .bind(target_commit)
                .bind(change_type)
                .bind(file_type)
                .bind(is_direct)
                .bind(commit.epoch);
        }
        query.execute(database.pool()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_modes_like_intval() {
        assert_eq!(octal("100644"), 0o100_644);
        // The old mode of a raw line starts with ":".
        assert_eq!(octal(":100644"), 0);
        assert_eq!(octal("160000"), 0o160_000);
    }

    #[test]
    fn parses_raw_diffs_like_php() {
        let raw = ":000000 100644 0000 1111 A\tdir/a.txt\n\
                   :100644 100644 2222 3333 M\tREADME\n\
                   :000000 120000 0000 4444 A\tlink\n\
                   :100644 100644 5555 5555 R100\told\tnew\n\
                   :100644 100644 6666 6666 C075\tsrc\tcopy\n";
        let changes = parse_raw_diff(raw).unwrap();
        let summary: Vec<(&str, i64, i64, bool)> = changes
            .values()
            .map(|change| {
                (
                    change.path.as_str(),
                    change.change_type,
                    change.file_type,
                    change.is_direct,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("/dir/a.txt", TYPE_ADD, FILE_NORMAL, true),
                ("/README", TYPE_CHANGE, FILE_NORMAL, true),
                ("/link", TYPE_ADD, FILE_SYMLINK, true),
                ("/new", TYPE_MOVE_HERE, FILE_NORMAL, true),
                ("/copy", TYPE_COPY_HERE, FILE_NORMAL, true),
                ("/", TYPE_CHILD, FILE_DIRECTORY, false),
                ("/src", TYPE_COPY_AWAY, FILE_NORMAL, false),
                ("/old", TYPE_MOVE_AWAY, FILE_NORMAL, true),
            ]
        );
        assert_eq!(changes["/new"].target_path.as_deref(), Some("/old"));
        assert!(changes["/new"].target_commit);
    }

    #[test]
    fn splits_like_preg_split() {
        assert_eq!(split_spaces("a  b c d e f", 5), ["a", "b", "c", "d", "e f"]);
        assert_eq!(split_spaces("a b", 5), ["a", "b"]);
    }
}
