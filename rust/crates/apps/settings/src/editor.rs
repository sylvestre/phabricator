//! `PhabricatorUserPreferencesEditor`: what saving a settings panel
//! writes, in one MySQL transaction, as PHP's editor does for the settings
//! transactions (`setting`) of a web form:
//!
//! - the `user_preferences` row, inserted when the user has none (PHP
//!   edits a synthetic object) and saved even when no transaction has an
//!   effect;
//! - the transactions with an effect, the others being dropped
//!   (`setContinueOnNoEffect(true)`);
//! - the deletion of the user's cached preferences (`applyFinalEffects()`);
//! - the `PhabricatorApplicationTransactionPublishWorker` task, which
//!   publishes nothing: preferences have no feed, mail or Herald.
//!
//! A value PHP refuses (a validation error, which PHP shows on the form)
//! is a [`Refusal`]: nothing is written, and the caller lets PHP answer.

use phab_core::{Phid, PhidType, hash, php, random};
use phab_db::editor::{self as db_editor, TransactionRow};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, DbError, rows, worker};
use serde_json::{Map, Value, json};

use crate::preferences::{self, identical};
use crate::setting::{self, Context, Setting};
use crate::{Error, PREFERENCES};

/// The content source of a web request (`PhabricatorWebContentSource`).
pub const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;
/// `PhabricatorUserPreferencesTransaction::TYPE_SETTING`.
const TYPE_SETTING: &str = "setting";
/// `PhabricatorUserPreferencesCacheType::KEY_PREFERENCES`.
pub const CACHE_KEY: &str = "user.preferences.v2";

/// One setting the form or panel sets, before the editor converts it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The setting.
    pub key: &'static str,
    /// The value as the form sent it (`null` for a missing field).
    pub value: Value,
    /// Whether the edit engine marks it as setting the value the form
    /// started with (`core.default`).
    pub is_default: bool,
}

/// Why PHP would not save the changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// A transaction with an effect.
#[derive(Debug)]
struct Planned {
    phid: String,
    key: &'static str,
    old: Value,
    new: Value,
    is_default: bool,
}

/// `expandSettingTransaction()`: changing the time zone also forgets the
/// offset the user chose to ignore.
fn expand(changes: Vec<Change>) -> Vec<Change> {
    let mut expanded = Vec::with_capacity(changes.len() + 1);
    for change in changes {
        let is_timezone = change.key == setting::TIMEZONE;
        expanded.push(change);
        if is_timezone {
            expanded.push(Change {
                key: setting::TIMEZONE_IGNORE_OFFSET,
                value: Value::Null,
                is_default: false,
            });
        }
    }
    expanded
}

/// The settings `json_encode()`d as PHP stores them: an empty array is
/// `[]`.
fn encode(values: &Map<String, Value>) -> String {
    php::json_encode(&php::round_trip(&Value::Object(values.clone())))
}

/// The task data of `queuePublishing()`.
fn publish_data(
    (object, actor): (&str, &str),
    transactions: &[String],
    is_new: bool,
    herald_header: Option<&str>,
) -> String {
    php::json_encode(&json!({
        "objectPHID": object,
        "actorPHID": actor,
        "xactionPHIDs": transactions,
        "state": {
            "parentMessageID": null,
            "isNewObject": is_new,
            "heraldEmailPHIDs": [],
            "heraldForcedEmailPHIDs": [],
            "heraldHeader": herald_header,
            "mailToPHIDs": [],
            "mailCCPHIDs": [],
            "feedNotifyPHIDs": [],
            "feedRelatedPHIDs": [],
            "feedShouldPublish": false,
            "mailShouldSend": false,
            "mustEncrypt": [],
            "mailStamps": [],
            "mailUnexpandablePHIDs": [],
            "mailMutedPHIDs": [],
            "webhookMap": [],
            "silent": null,
            "sendHistory": false,
            "excludeMailRecipientPHIDs": [],
            "custom": [],
            "custom.encoding": [],
        },
    }))
}

/// The transactions of `changes` with an effect on `values`, which they
/// then update. Old values come from the object before any transaction
/// applies.
fn plan(
    planned_settings: Vec<(&Setting, Change)>,
    values: &mut Map<String, Value>,
) -> Vec<Planned> {
    let mut planned = Vec::new();
    for (setting, change) in planned_settings {
        let old = values.get(change.key).cloned().unwrap_or(Value::Null);
        let new = setting.new_value(&change.value);
        if identical(&old, &new) {
            continue;
        }
        planned.push(Planned {
            phid: Phid::generate(PhidType::TRANSACTION, Some(PREFERENCES)).into_string(),
            key: change.key,
            old,
            new,
            is_default: change.is_default,
        });
    }
    for planned in &planned {
        if planned.new.is_null() {
            values.shift_remove(planned.key);
        } else {
            values.insert(planned.key.to_owned(), planned.new.clone());
        }
    }
    planned
}

/// `PhabricatorUserPreferences::save()`: updates the row, or inserts it
/// for a user who had none. Returns the PHID of the preferences.
async fn save(
    connection: &mut MySqlConnection,
    database: &Database,
    (user, existing): (&Phid, Option<(i64, String)>),
    values: &Map<String, Value>,
    now: i64,
) -> Result<String, Error> {
    let table = database.table("user", "user_preferences");
    let encoded = encode(values);
    if let Some((id, phid)) = existing {
        phab_db::sqlx::query(&format!(
            "UPDATE {table} SET preferences = ?, dateModified = ? WHERE id = ?"
        ))
        .bind(&encoded)
        .bind(now)
        .bind(id)
        .execute(&mut *connection)
        .await
        .map_err(DbError::from)?;
        return Ok(phid);
    }
    let phid = Phid::generate(PREFERENCES, None).into_string();
    phab_db::sqlx::query(&format!(
        "INSERT INTO {table} (userPHID, preferences, dateCreated, dateModified, phid, \
         builtinKey) VALUES (?, ?, ?, ?, ?, NULL)"
    ))
    .bind(user.as_str())
    .bind(&encoded)
    .bind(now)
    .bind(now)
    .bind(&phid)
    .execute(&mut *connection)
    .await
    .map_err(DbError::from)?;
    Ok(phid)
}

/// The transaction rows, sharing one group ID.
async fn insert_transactions(
    connection: &mut MySqlConnection,
    database: &Database,
    (actor, object): (&Phid, &str),
    planned: &[Planned],
    now: i64,
) -> Result<(), Error> {
    let group_id = random::random_characters(32);
    let table = database.table("user", "user_preferencestransaction");
    for planned in planned {
        let mut metadata = Map::new();
        metadata.insert("setting.key".to_owned(), json!(planned.key));
        if planned.is_default {
            metadata.insert("core.default".to_owned(), json!(true));
        }
        metadata.insert("core.groupID".to_owned(), json!(group_id));
        let metadata = php::json_encode(&Value::Object(metadata));
        let old = php::json_encode(&php::round_trip(&planned.old));
        let new = php::json_encode(&php::round_trip(&planned.new));
        let row = TransactionRow {
            phid: &planned.phid,
            author: actor.as_str(),
            object,
            kind: TYPE_SETTING,
            old: &old,
            new: &new,
            metadata: &metadata,
            content_source: WEB_SOURCE,
        };
        db_editor::insert_transaction(connection, &table, row, now).await?;
    }
    Ok(())
}

/// `applyFinalEffects()` and `queuePublishing()`: forgets the cached
/// preferences of `user` and queues the publish task.
async fn finish(
    connection: &mut MySqlConnection,
    database: &Database,
    (actor, user, object): (&Phid, &Phid, &str),
    (phids, is_new): (&[String], bool),
    now: i64,
) -> Result<(), Error> {
    // `PhabricatorUserCache::clearCache()`.
    phab_db::sqlx::query(&format!(
        "DELETE FROM {} WHERE cacheIndex = ? AND userPHID IN (?)",
        database.table("user", "user_cache")
    ))
    .bind(hash::digest_for_index(CACHE_KEY.as_bytes()))
    .bind(user.as_str())
    .execute(&mut *connection)
    .await
    .map_err(DbError::from)?;

    // `loadXHeraldRulesHeader()`.
    let header = phab_db::sqlx::query(&format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    ))
    .bind(object)
    .fetch_optional(&mut *connection)
    .await
    .map_err(DbError::from)?
    .map(|row| rows::text(&row, "header"))
    .transpose()?;

    let data = publish_data((object, actor.as_str()), phids, is_new, header.as_deref());
    let task = worker::Task {
        class: "PhabricatorApplicationTransactionPublishWorker",
        data: &data,
        priority: worker::PRIORITY_ALERTS,
        object_phid: Some(object),
    };
    worker::schedule_task(connection, database, task, now).await?;
    Ok(())
}

/// Applies `changes` to the preferences of `user` as `actor` (the user
/// themself), like `PhabricatorUserPreferencesEditor::applyTransactions()`.
/// Returns the PHIDs of the saved transactions.
pub async fn apply(
    database: &Database,
    context: &Context<'_>,
    (actor, user): (&Phid, &Phid),
    changes: Vec<Change>,
    now: i64,
) -> Result<Result<Vec<String>, Refusal>, Error> {
    let changes = expand(changes);
    let mut planned_settings: Vec<(&Setting, Change)> = Vec::with_capacity(changes.len());
    for change in changes {
        let setting =
            setting::find(change.key).ok_or(Error::Unsupported("a setting Rust doesn't know"))?;
        if let Err(message) = setting.validate(context, &change.value) {
            return Ok(Err(Refusal(message)));
        }
        planned_settings.push((setting, change));
    }

    let mut transaction = database.pool().begin().await.map_err(DbError::from)?;
    let connection = &mut *transaction;
    // `reload()` under `beginReadLocking()`.
    let row = phab_db::sqlx::query(&format!(
        "SELECT id, phid, preferences FROM {} WHERE userPHID = ? FOR UPDATE",
        database.table("user", "user_preferences")
    ))
    .bind(user.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(DbError::from)?;
    let (existing, mut values) = match row {
        Some(row) => (
            Some((rows::int(&row, "id")?, rows::text(&row, "phid")?)),
            preferences::decode(rows::json(&row, "preferences")?)?,
        ),
        None => (None, Map::new()),
    };
    let is_new = existing.is_none();

    let planned = plan(planned_settings, &mut values);
    let object = save(connection, database, (user, existing), &values, now).await?;
    insert_transactions(connection, database, (actor, &object), &planned, now).await?;
    let phids: Vec<String> = planned.iter().map(|planned| planned.phid.clone()).collect();
    finish(
        connection,
        database,
        (actor, user, &object),
        (&phids, is_new),
        now,
    )
    .await?;
    transaction.commit().await.map_err(DbError::from)?;
    Ok(Ok(phids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_zone_changes_forget_the_ignored_offset() {
        let change = |key| Change {
            key,
            value: json!("x"),
            is_default: false,
        };
        let keys: Vec<_> = expand(vec![change("timezone"), change("date-format")])
            .into_iter()
            .map(|change| change.key)
            .collect();
        assert_eq!(keys, ["timezone", "time.offset.ignore", "date-format"]);
    }

    #[test]
    fn settings_are_encoded_like_php() {
        assert_eq!(encode(&Map::new()), "[]");
        let mut values = Map::new();
        values.insert("timezone".to_owned(), json!("Europe/Paris"));
        values.insert("mailtags".to_owned(), json!({"a-b": 1}));
        assert_eq!(
            encode(&values),
            r#"{"timezone":"Europe\/Paris","mailtags":{"a-b":1}}"#
        );
    }
}
