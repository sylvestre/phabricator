//! `PhabricatorConfigEntry`: the `config_entry` rows of the `default`
//! namespace, which `PhabricatorConfigDatabaseSource` reads and
//! `bin/config --database` writes without transactions.

use phab_core::{Phid, PhidType};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use serde_json::Value;

/// `PhabricatorConfigConfigPHIDType`.
pub const CONFIG_PHID: PhidType = PhidType::new(*b"CONF");

/// The namespace PHP reads and writes.
const NAMESPACE: &str = "default";

/// A `config_entry` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigEntry {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The option key.
    pub key: String,
    /// The value, decoded.
    pub value: Value,
    /// Deleted entries are kept, and ignored.
    pub is_deleted: bool,
    /// When it was written first.
    pub date_created: i64,
    /// When it was written last.
    pub date_modified: i64,
}

impl ConfigEntry {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            key: rows::text(row, "configKey")?,
            value: rows::json(row, "value")?,
            is_deleted: rows::flag(row, "isDeleted")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
        })
    }
}

const COLUMNS: &str = "id, phid, configKey, value, isDeleted, dateCreated, dateModified";

/// Every entry of the `default` namespace, deleted ones included
/// (`loadAllWhere('namespace = %s', 'default')`).
pub async fn load_all(database: &Database) -> Result<Vec<ConfigEntry>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE namespace = ? ORDER BY id",
        database.table("config", "config_entry")
    );
    phab_db::sqlx::query(&sql)
        .bind(NAMESPACE)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(ConfigEntry::from_row)
        .collect()
}

/// The entry of `key`, deleted or not.
pub async fn load(database: &Database, key: &str) -> Result<Option<ConfigEntry>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE configKey = ? AND namespace = ?",
        database.table("config", "config_entry")
    );
    phab_db::sqlx::query(&sql)
        .bind(key)
        .bind(NAMESPACE)
        .fetch_optional(database.pool())
        .await?
        .as_ref()
        .map(ConfigEntry::from_row)
        .transpose()
}

/// `PhabricatorConfigDatabaseSource::getKeys()`: the value of `key`, if an
/// entry which isn't deleted sets it.
pub async fn value(database: &Database, key: &str) -> Result<Option<Value>, DbError> {
    Ok(load(database, key)
        .await?
        .filter(|entry| !entry.is_deleted)
        .map(|entry| entry.value))
}

/// `PhabricatorConfigEntry::loadConfigEntry($key)`, then `setValue()`,
/// `setIsDeleted(0)` and `save()`: updates the entry, or inserts one.
pub async fn set(database: &Database, key: &str, value: &Value, now: i64) -> Result<(), DbError> {
    // The serialization of a PHP array: `json_encode()` of what
    // `json_decode()` gave.
    let encoded = phab_core::php::json_encode(&phab_core::php::round_trip(value));
    let table = database.table("config", "config_entry");
    if let Some(entry) = load(database, key).await? {
        let sql =
            format!("UPDATE {table} SET value = ?, isDeleted = 0, dateModified = ? WHERE id = ?");
        phab_db::sqlx::query(&sql)
            .bind(encoded)
            .bind(now)
            .bind(entry.id)
            .execute(database.pool())
            .await?;
    } else {
        let sql = format!(
            "INSERT INTO {table} (phid, namespace, configKey, value, isDeleted, dateCreated, \
             dateModified) VALUES (?, ?, ?, ?, 0, ?, ?)"
        );
        phab_db::sqlx::query(&sql)
            .bind(Phid::generate(CONFIG_PHID, None).as_str())
            .bind(NAMESPACE)
            .bind(key)
            .bind(encoded)
            .bind(now)
            .bind(now)
            .execute(database.pool())
            .await?;
    }
    Ok(())
}

/// `setIsDeleted(1)` and `save()` of the entry of `key`; false when no
/// entry sets it.
pub async fn delete(database: &Database, key: &str, now: i64) -> Result<bool, DbError> {
    let Some(entry) = load(database, key).await?.filter(|entry| !entry.is_deleted) else {
        return Ok(false);
    };
    let sql = format!(
        "UPDATE {} SET isDeleted = 1, dateModified = ? WHERE id = ?",
        database.table("config", "config_entry")
    );
    phab_db::sqlx::query(&sql)
        .bind(now)
        .bind(entry.id)
        .execute(database.pool())
        .await?;
    Ok(true)
}
