//! Rows of `differential_diff`, `differential_changeset` and
//! `differential_hunk`, and the hunk data formats of `DifferentialHunk`.
//!
//! These loaders read as the omnipotent viewer: diffs, changesets and hunks
//! take their policy from the revision, which the caller checks.

use std::io::Read;

use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use serde_json::Value;
use thiserror::Error;

use crate::changeset::{ChangeType, Changeset, FileType};
use crate::hunk::Hunk;

/// Why a diff could not be loaded the way PHP loads it.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A query failed.
    #[error(transparent)]
    Database(#[from] DbError),
    /// `dataType` is neither `text` nor `file`.
    #[error("hunk {hunk} has unsupported data type {data_type:?}")]
    DataType {
        /// The hunk ID.
        hunk: i64,
        /// The stored value.
        data_type: String,
    },
    /// `dataFormat` is neither `byte` nor `gzde`.
    #[error("hunk {hunk} has unsupported data format {data_format:?}")]
    DataFormat {
        /// The hunk ID.
        hunk: i64,
        /// The stored value.
        data_format: String,
    },
    /// Deflated data that doesn't inflate.
    #[error("hunk {hunk} has data which does not inflate")]
    Inflate {
        /// The hunk ID.
        hunk: i64,
    },
    /// The hunk is not UTF-8. PHP guesses the encoding with
    /// `mb_detect_encoding()`, which is not ported.
    #[error("hunk {hunk} is not UTF-8 (encoding {encoding:?})")]
    Encoding {
        /// The hunk ID.
        hunk: i64,
        /// `dataEncoding`.
        encoding: Option<String>,
    },
    /// The data is in a file Rust can't read: missing, or in a storage
    /// engine or format other than MySQL blobs stored raw.
    #[error("hunk {hunk} has its data in file {file} which can't be read here: {reason}")]
    File {
        /// The hunk ID.
        hunk: i64,
        /// The file PHID.
        file: String,
        /// What is missing or unsupported.
        reason: String,
    },
}

impl From<phab_db::sqlx::Error> for StorageError {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Database(error.into())
    }
}

/// A `differential_diff` row, as much of `DifferentialDiff` as rendering
/// needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// The row ID.
    pub id: i64,
    /// The diff PHID.
    pub phid: String,
    /// The revision it belongs to, if any.
    pub revision_id: Option<i64>,
    /// Who created it.
    pub author_phid: Option<String>,
    /// The repository, if known.
    pub repository_phid: Option<String>,
    /// The view policy, used while the diff has no revision.
    pub view_policy: String,
    /// `sourceControlBaseRevision`.
    pub base_revision: Option<String>,
    /// `dateCreated`.
    pub date_created: i64,
}

/// Loads the diff with this ID.
pub async fn load_diff(database: &Database, id: i64) -> Result<Option<Diff>, StorageError> {
    load_diff_where(database, "id = ?", id.to_string()).await
}

/// The diff with this PHID, as a revision's `activeDiffPHID` names it.
pub async fn load_diff_by_phid(
    database: &Database,
    phid: &str,
) -> Result<Option<Diff>, StorageError> {
    load_diff_where(database, "phid = ?", phid.to_owned()).await
}

async fn load_diff_where(
    database: &Database,
    condition: &str,
    value: String,
) -> Result<Option<Diff>, StorageError> {
    let sql = format!(
        "SELECT id, phid, revisionID, authorPHID, repositoryPHID, viewPolicy, \
           sourceControlBaseRevision, dateCreated FROM {} WHERE {condition}",
        database.table("differential", "differential_diff")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(value)
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(Diff {
        id: rows::int(&row, "id")?,
        phid: rows::text(&row, "phid")?,
        revision_id: rows::opt_int(&row, "revisionID")?,
        author_phid: rows::opt_text(&row, "authorPHID")?,
        repository_phid: rows::opt_text(&row, "repositoryPHID")?,
        view_policy: rows::text(&row, "viewPolicy")?,
        base_revision: rows::opt_text(&row, "sourceControlBaseRevision")?,
        date_created: rows::int(&row, "dateCreated")?,
    }))
}

fn changeset_from_row(row: &MySqlRow) -> Result<Changeset, DbError> {
    let away_paths = match rows::json(row, "awayPaths")? {
        Value::Array(paths) => paths
            .into_iter()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Changeset {
        id: rows::int(row, "id")?,
        phid: rows::opt_text(row, "phid")?.filter(|phid| !phid.is_empty()),
        diff_id: rows::int(row, "diffID")?,
        old_file: rows::opt_text(row, "oldFile")?,
        filename: rows::text(row, "filename")?,
        away_paths,
        change_type: ChangeType::from_value(rows::int(row, "changeType")?),
        file_type: FileType::from_value(rows::int(row, "fileType")?),
        metadata: rows::json(row, "metadata")?,
        old_properties: rows::json(row, "oldProperties")?,
        new_properties: rows::json(row, "newProperties")?,
        add_lines: rows::int(row, "addLines")?,
        del_lines: rows::int(row, "delLines")?,
        hunks: Vec::new(),
    })
}

/// `DifferentialChangesetQuery` for one diff, with `needHunks(true)`.
/// Changesets come newest first, as the query's default order; pages sort
/// them with [`Changeset::sort_key`]. Hunks come in ID order.
pub async fn load_changesets(
    database: &Database,
    diff_id: i64,
) -> Result<Vec<Changeset>, StorageError> {
    let sql = format!(
        "SELECT id, phid, diffID, oldFile, filename, awayPaths, changeType, fileType, \
           metadata, oldProperties, newProperties, addLines, delLines \
         FROM {} WHERE diffID = ? ORDER BY id DESC",
        database.table("differential", "differential_changeset")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(diff_id)
        .fetch_all(database.pool())
        .await?;
    let mut changesets = found
        .iter()
        .map(changeset_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let ids: Vec<i64> = changesets.iter().map(|changeset| changeset.id).collect();
    for (changeset_id, hunk) in load_hunks(database, &ids).await? {
        if let Some(changeset) = changesets
            .iter_mut()
            .find(|changeset| changeset.id == changeset_id)
        {
            changeset.hunks.push(hunk);
        }
    }
    Ok(changesets)
}

/// The data format of a hunk (`DifferentialHunk::DATAFORMAT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataFormat {
    /// `byte`: stored as is.
    Raw,
    /// `gzde`: `gzdeflate()`d.
    Deflated,
}

/// `DifferentialHunkQuery` for these changesets: the hunks with their
/// changeset ID, in ID order.
async fn load_hunks(
    database: &Database,
    changeset_ids: &[i64],
) -> Result<Vec<(i64, Hunk)>, StorageError> {
    if changeset_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, changesetID, oldOffset, oldLen, newOffset, newLen, dataType, \
           dataEncoding, dataFormat, data FROM {} WHERE changesetID IN ({}) ORDER BY id",
        database.table("differential", "differential_hunk"),
        phab_db::placeholders(changeset_ids.len()),
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), changeset_ids.iter().copied())
        .fetch_all(database.pool())
        .await?;
    let mut hunks = Vec::with_capacity(found.len());
    for row in &found {
        let id = rows::int(row, "id")?;
        let changes = hunk_changes(database, row, id).await?;
        hunks.push((
            rows::int(row, "changesetID")?,
            Hunk {
                old_offset: rows::int(row, "oldOffset")?,
                old_len: rows::int(row, "oldLen")?,
                new_offset: rows::int(row, "newOffset")?,
                new_len: rows::int(row, "newLen")?,
                changes,
            },
        ));
    }
    Ok(hunks)
}

/// `DifferentialHunk::getChanges()`: the stored data, read from a file if
/// needed, inflated, then decoded.
async fn hunk_changes(
    database: &Database,
    row: &MySqlRow,
    hunk: i64,
) -> Result<String, StorageError> {
    let data_type = rows::text(row, "dataType")?;
    let data_format = rows::text(row, "dataFormat")?;
    let format = match data_format.as_str() {
        "byte" => DataFormat::Raw,
        "gzde" => DataFormat::Deflated,
        _ => return Err(StorageError::DataFormat { hunk, data_format }),
    };
    let stored: Vec<u8> = phab_db::sqlx::Row::try_get(row, "data")?;
    let data = match data_type.as_str() {
        "text" => stored,
        "file" => {
            let file = String::from_utf8_lossy(&stored).into_owned();
            load_file_data(database, hunk, &file).await?
        }
        _ => return Err(StorageError::DataType { hunk, data_type }),
    };
    let raw = match format {
        DataFormat::Raw => data,
        DataFormat::Deflated => inflate(&data).ok_or(StorageError::Inflate { hunk })?,
    };
    decode(raw, rows::opt_text(row, "dataEncoding")?, hunk)
}

/// `PhabricatorCaches::inflateData()`: raw DEFLATE, as `gzinflate()`.
pub fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut inflated = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut inflated)
        .ok()?;
    Some(inflated)
}

/// `PhabricatorLiskDAO::getUTF8StringFromStorage()` for the encodings
/// Rust supports: `utf8`, or empty data.
fn decode(raw: Vec<u8>, encoding: Option<String>, hunk: i64) -> Result<String, StorageError> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    if encoding.as_deref() != Some("utf8") {
        return Err(StorageError::Encoding { hunk, encoding });
    }
    String::from_utf8(raw).map_err(|_| StorageError::Encoding { hunk, encoding })
}

/// `PhabricatorFile::loadFileData()` for files stored raw in MySQL blobs
/// (`PhabricatorMySQLFileStorageEngine`), the engine hunks are moved to by
/// `bin/differential migrate-hunk`.
async fn load_file_data(
    database: &Database,
    hunk: i64,
    file: &str,
) -> Result<Vec<u8>, StorageError> {
    let unreadable = |reason: String| StorageError::File {
        hunk,
        file: file.to_owned(),
        reason,
    };
    let sql = format!(
        "SELECT storageEngine, storageFormat, storageHandle FROM {} WHERE phid = ?",
        database.table("file", "file")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(file)
        .fetch_optional(database.pool())
        .await?
        .ok_or_else(|| unreadable("no such file".to_owned()))?;
    let engine = rows::text(&row, "storageEngine")?;
    let format = rows::text(&row, "storageFormat")?;
    if engine != "blob" || format != "raw" {
        return Err(unreadable(format!(
            "storage engine {engine:?}, format {format:?}"
        )));
    }
    let handle = rows::text(&row, "storageHandle")?;
    let sql = format!(
        "SELECT data FROM {} WHERE id = ?",
        database.table("file", "file_storageblob")
    );
    let blob: Option<Vec<u8>> = phab_db::sqlx::query_scalar(&sql)
        .bind(&handle)
        .fetch_optional(database.pool())
        .await?;
    blob.ok_or_else(|| unreadable(format!("no blob {handle}")))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn deflated_data_inflates() {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b" a\n-b\n+c\n").unwrap();
        let deflated = encoder.finish().unwrap();
        assert_eq!(inflate(&deflated).unwrap(), b" a\n-b\n+c\n");
        assert_eq!(inflate(b"not deflate data"), None);
    }

    #[test]
    fn only_utf8_is_decoded() {
        assert_eq!(decode(Vec::new(), None, 1).unwrap(), "");
        assert_eq!(
            decode(b"+x\n".to_vec(), Some("utf8".to_owned()), 1).unwrap(),
            "+x\n"
        );
        assert!(matches!(
            decode(b"+\xe9\n".to_vec(), None, 1),
            Err(StorageError::Encoding { .. })
        ));
        assert!(matches!(
            decode(b"+\xe9\n".to_vec(), Some("utf8".to_owned()), 1),
            Err(StorageError::Encoding { .. })
        ));
    }
}
