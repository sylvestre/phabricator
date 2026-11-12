//! Typed reads from `MySQL` rows.
//!
//! PHP reads every column as a string (or `null`), so callers which echo
//! database values into API responses must keep them as strings. These
//! helpers accept the column types the Phabricator schema uses (`varbinary`
//! for PHIDs, signed and unsigned integers of every width).

use phab_core::Phid;
use serde_json::Value;
use sqlx::Row;
use sqlx::mysql::MySqlRow;

use crate::DbError;

fn column_error(column: &str) -> DbError {
    DbError::ColumnType {
        column: column.to_owned(),
    }
}

/// A text or binary column, decoded as lossy UTF-8.
pub fn opt_text(row: &MySqlRow, column: &str) -> Result<Option<String>, DbError> {
    if let Ok(value) = row.try_get::<Option<Vec<u8>>, _>(column) {
        return Ok(value.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()));
    }
    row.try_get::<Option<String>, _>(column)
        .map_err(|_| column_error(column))
}

/// A non-null text or binary column; see [`opt_text`].
pub fn text(row: &MySqlRow, column: &str) -> Result<String, DbError> {
    opt_text(row, column)?.ok_or_else(|| column_error(column))
}

/// An integer or boolean column of any width.
pub fn opt_int(row: &MySqlRow, column: &str) -> Result<Option<i64>, DbError> {
    if let Ok(value) = row.try_get::<Option<i64>, _>(column) {
        return Ok(value);
    }
    if let Ok(value) = row.try_get::<Option<u64>, _>(column) {
        return value
            .map(|number| i64::try_from(number).map_err(|_| column_error(column)))
            .transpose();
    }
    if let Ok(value) = row.try_get::<Option<u32>, _>(column) {
        return Ok(value.map(i64::from));
    }
    if let Ok(value) = row.try_get::<Option<i32>, _>(column) {
        return Ok(value.map(i64::from));
    }
    if let Ok(value) = row.try_get::<Option<i8>, _>(column) {
        return Ok(value.map(i64::from));
    }
    if let Ok(value) = row.try_get::<Option<u8>, _>(column) {
        return Ok(value.map(i64::from));
    }
    if let Ok(value) = row.try_get::<Option<bool>, _>(column) {
        return Ok(value.map(i64::from));
    }
    Err(column_error(column))
}

/// A non-null integer column; see [`opt_int`].
pub fn int(row: &MySqlRow, column: &str) -> Result<i64, DbError> {
    opt_int(row, column)?.ok_or_else(|| column_error(column))
}

/// A PHID column where PHP treats an empty string like `null`.
pub fn opt_phid(row: &MySqlRow, column: &str) -> Result<Option<Phid>, DbError> {
    Ok(opt_text(row, column)?
        .filter(|phid| !phid.is_empty())
        .map(Phid::new))
}

/// An unsigned column, including `bigint unsigned` values above `i64::MAX`.
pub fn uint(row: &MySqlRow, column: &str) -> Result<u64, DbError> {
    if let Ok(value) = row.try_get::<u64, _>(column) {
        return Ok(value);
    }
    u64::try_from(int(row, column)?).map_err(|_| column_error(column))
}

/// A boolean column read with PHP truthiness of its integer value.
pub fn flag(row: &MySqlRow, column: &str) -> Result<bool, DbError> {
    Ok(opt_int(row, column)?.unwrap_or(0) != 0)
}

/// A column with `LiskDAO::SERIALIZATION_JSON`. PHP decodes `null` and empty
/// strings to `null`.
pub fn json(row: &MySqlRow, column: &str) -> Result<Value, DbError> {
    match opt_text(row, column)? {
        None => Ok(Value::Null),
        Some(text) if text.is_empty() => Ok(Value::Null),
        Some(text) => serde_json::from_str(&text).map_err(|source| DbError::Json {
            column: column.to_owned(),
            source,
        }),
    }
}
