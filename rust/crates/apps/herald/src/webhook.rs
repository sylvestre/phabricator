//! Herald webhooks (`HeraldWebhook`) and their requests
//! (`HeraldWebhookRequest`): what the webhook page reads, the test request
//! `HeraldWebhookTestController` queues, and the key
//! `HeraldWebhookKeyController` regenerates (`regenerateHMACKey()`, saved
//! without a transaction).

use phab_core::{Phid, PhidType, php, random};
use phab_db::{Database, DbError, rows, worker};
use serde_json::{Map, Value, json};

/// `HeraldWebhookRequestPHIDType`.
const REQUEST_PHID_TYPE: PhidType = PhidType::new(*b"HWBR");

/// `HeraldRuleActionAffectsObjectEdgeType`.
const RULE_AFFECTS_OBJECT: i64 = 69;

/// A `herald_webhook` row, with what its page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// Where requests go.
    pub webhook_uri: String,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// `firehose`, `enabled` or `disabled`.
    pub status: String,
    /// The key requests are signed with.
    pub hmac_key: String,
}

/// The webhook with this ID.
///
/// # Errors
///
/// When the query fails.
pub async fn load(database: &Database, id: i64) -> Result<Option<Webhook>, DbError> {
    let sql = format!(
        "SELECT id, phid, name, webhookURI, viewPolicy, editPolicy, status, hmacKey FROM {} \
         WHERE id = ?",
        database.table("herald", "herald_webhook"),
    );
    phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?
        .map(|row| {
            Ok(Webhook {
                id: rows::int(&row, "id")?,
                phid: Phid::new(rows::text(&row, "phid")?),
                name: rows::text(&row, "name")?,
                webhook_uri: rows::text(&row, "webhookURI")?,
                view_policy: rows::text(&row, "viewPolicy")?,
                edit_policy: rows::text(&row, "editPolicy")?,
                status: rows::text(&row, "status")?,
                hmac_key: rows::text(&row, "hmacKey")?,
            })
        })
        .transpose()
}

/// A `herald_webhookrequest` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The row ID.
    pub id: i64,
    /// The object the request is about.
    pub object_phid: Phid,
    /// `queued`, `failed` or `sent`.
    pub status: String,
    /// `getErrorType()`.
    pub error_type: Option<String>,
    /// `getErrorCode()`, as `%s` prints it.
    pub error_code: Option<String>,
    /// `lastRequestEpoch`.
    pub last_request_epoch: i64,
}

/// The newest `limit` requests of `webhook` (`HeraldWebhookRequestQuery`).
///
/// # Errors
///
/// When the query fails.
pub async fn requests(
    database: &Database,
    webhook: &Phid,
    limit: usize,
) -> Result<Vec<Request>, DbError> {
    let sql = format!(
        "SELECT id, objectPHID, status, properties, lastRequestEpoch FROM {} \
         WHERE webhookPHID = ? ORDER BY id DESC LIMIT {limit}",
        database.table("herald", "herald_webhookrequest"),
    );
    phab_db::sqlx::query(&sql)
        .bind(webhook.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            let properties = rows::json(row, "properties")?;
            let text = |key: &str| match properties.get(key) {
                None | Some(Value::Null) => None,
                Some(value) => Some(php::string_cast(value).into_owned()),
            };
            Ok(Request {
                id: rows::int(row, "id")?,
                object_phid: Phid::new(rows::text(row, "objectPHID")?),
                status: rows::text(row, "status")?,
                error_type: text("errorType"),
                error_code: text("errorCode"),
                last_request_epoch: rows::int(row, "lastRequestEpoch")?,
            })
        })
        .collect()
}

/// `isInErrorBackoff()`: at least 10 requests failed in the last 5
/// minutes.
///
/// # Errors
///
/// When the query fails.
pub async fn is_in_error_backoff(
    database: &Database,
    webhook: &Phid,
    now: i64,
) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT COUNT(*) AS failures FROM {} WHERE webhookPHID = ? AND lastRequestResult = \
         'fail' AND lastRequestEpoch >= ?",
        database.table("herald", "herald_webhookrequest"),
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(webhook.as_str())
        .bind(now - 300)
        .fetch_one(database.pool())
        .await?;
    Ok(rows::int(&row, "failures")? >= 10)
}

/// The enabled rules with an action on `webhook`
/// (`withAffectedObjectPHIDs()`), newest first, at most `limit`.
///
/// # Errors
///
/// When the query fails.
pub async fn calling_rules(
    database: &Database,
    webhook: &Phid,
    limit: usize,
) -> Result<Vec<i64>, DbError> {
    let sql = format!(
        "SELECT DISTINCT r.id FROM {} r JOIN {} e ON e.src = r.phid AND e.type = ? \
         WHERE e.dst = ? AND r.isDisabled = 0 ORDER BY r.id DESC LIMIT {limit}",
        database.table("herald", "herald_rule"),
        database.table("herald", "edge"),
    );
    phab_db::sqlx::query(&sql)
        .bind(RULE_AFFECTS_OBJECT)
        .bind(webhook.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| rows::int(row, "id"))
        .collect()
}

/// Queues a test request (`HeraldWebhookTestController`): the request
/// about `object`, triggered by `actor`, listing `transactions`, and the
/// `HeraldWebhookWorker` task which sends it. Returns the request ID.
///
/// # Errors
///
/// When a query fails.
pub async fn queue_test_request(
    database: &Database,
    webhook: &Phid,
    (object, actor, transactions): (&str, &str, &[String]),
    now: i64,
) -> Result<i64, DbError> {
    let phid = Phid::generate(REQUEST_PHID_TYPE, None).into_string();
    let mut properties = Map::new();
    properties.insert("retry".to_owned(), json!("never"));
    properties.insert("triggerPHIDs".to_owned(), json!([actor]));
    properties.insert("test".to_owned(), json!(true));
    properties.insert("transactionPHIDs".to_owned(), json!(transactions));
    let mut transaction = database.pool().begin().await?;
    let id = phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, webhookPHID, objectPHID, status, properties, lastRequestResult, \
         lastRequestEpoch, dateCreated, dateModified) VALUES (?, ?, ?, 'queued', ?, 'none', 0, \
         ?, ?)",
        database.table("herald", "herald_webhookrequest"),
    ))
    .bind(&phid)
    .bind(webhook.as_str())
    .bind(object)
    .bind(php::json_encode(&Value::Object(properties)))
    .bind(now)
    .bind(now)
    .execute(&mut *transaction)
    .await?
    .last_insert_id();
    let data = php::json_encode(&json!({"webhookRequestPHID": phid}));
    let task = worker::Task {
        class: "HeraldWebhookWorker",
        data: &data,
        priority: worker::PRIORITY_DEFAULT,
        object_phid: Some(&phid),
    };
    worker::schedule_task(&mut transaction, database, task, now).await?;
    transaction.commit().await?;
    Ok(i64::try_from(id).unwrap_or_default())
}

/// `regenerateHMACKey()->save()`: a new key, saved with every column as
/// Lisk does. Returns the key.
///
/// # Errors
///
/// When the query fails.
pub async fn regenerate_key(database: &Database, id: i64, now: i64) -> Result<String, DbError> {
    let key = random::random_characters(32);
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET hmacKey = ?, dateModified = ? WHERE id = ?",
        database.table("herald", "herald_webhook"),
    ))
    .bind(&key)
    .bind(now)
    .bind(id)
    .execute(database.pool())
    .await?;
    Ok(key)
}

/// The newest `limit` transaction PHIDs of a webhook
/// (`HeraldWebhookTransactionQuery`), for its own test requests.
///
/// # Errors
///
/// When the query fails.
pub async fn transaction_phids(
    database: &Database,
    webhook: &Phid,
    limit: usize,
) -> Result<Vec<String>, DbError> {
    let sql = format!(
        "SELECT phid FROM {} WHERE objectPHID = ? ORDER BY id DESC LIMIT {limit}",
        database.table("herald", "herald_webhooktransaction"),
    );
    phab_db::sqlx::query(&sql)
        .bind(webhook.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| rows::text(row, "phid"))
        .collect()
}
