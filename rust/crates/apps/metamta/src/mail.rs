//! `PhabricatorMetaMTAMail` rows (`metamta_mail`).

use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value};

use crate::Error;

/// `PhabricatorMailOutboundStatus::STATUS_QUEUE`.
pub const STATUS_QUEUE: &str = "queued";
/// `PhabricatorMailOutboundStatus::STATUS_SENT`.
pub const STATUS_SENT: &str = "sent";
/// `PhabricatorMailOutboundStatus::STATUS_FAIL`.
pub const STATUS_FAIL: &str = "fail";
/// `PhabricatorMailOutboundStatus::STATUS_VOID`.
pub const STATUS_VOID: &str = "void";

/// A mail row, its `parameters` decoded as PHP decodes them.
#[derive(Debug, Clone, PartialEq)]
pub struct Mail {
    /// `id`.
    pub id: i64,
    /// `phid`.
    pub phid: String,
    /// `actorPHID`.
    pub actor_phid: Option<String>,
    /// `parameters`, keys in their stored order.
    pub parameters: Map<String, Value>,
    /// `status`.
    pub status: String,
    /// `message`.
    pub message: Option<String>,
    /// `relatedPHID`.
    pub related_phid: Option<String>,
}

impl Mail {
    /// `getParam($param)`: `None` for a missing key, which takes the
    /// default; a stored `null` is returned as is, as `idx()` does.
    pub fn param(&self, key: &str) -> Option<&Value> {
        self.parameters.get(key)
    }

    /// `setParam()`: an existing key keeps its place.
    pub fn set_param(&mut self, key: &str, value: Value) {
        self.parameters.insert(key.to_owned(), value);
    }

    /// Whether a parameter is set and truthy.
    pub fn flag(&self, key: &str) -> bool {
        self.param(key).is_some_and(phab_core::php::is_truthy)
    }

    /// `getParam($key)` as a string, `None` when missing; `Err` when it is
    /// another type.
    pub fn string_param(&self, key: &str) -> Result<Option<&str>, Error> {
        match self.param(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(Error::unsupported(format!(
                "mail parameter {key} is not a string"
            ))),
        }
    }

    /// A list parameter of strings, empty when missing (a stored `null`
    /// makes PHP fail).
    pub fn strings_param(&self, key: &str) -> Result<Vec<String>, Error> {
        match self.param(key) {
            None => Ok(Vec::new()),
            Some(value) => crate::php::strings(value).ok_or_else(|| {
                Error::unsupported(format!("mail parameter {key} is not a list of strings"))
            }),
        }
    }

    /// `getURI()`.
    pub fn uri(&self) -> String {
        format!("/mail/detail/{}/", self.id)
    }
}

/// Loads mail `id` (`PhabricatorMetaMTAMail::load()`).
pub async fn load(database: &Database, id: i64) -> Result<Option<Mail>, Error> {
    let sql = format!(
        "SELECT id, phid, actorPHID, parameters, status, message, relatedPHID FROM {} \
         WHERE id = ?",
        database.table("metamta", "metamta_mail")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
    else {
        return Ok(None);
    };
    let parameters = match rows::json(&row, "parameters") {
        Ok(Value::Object(map)) => map,
        Ok(Value::Array(list)) if list.is_empty() => Map::new(),
        Ok(_) | Err(DbError::Json { .. }) => {
            return Err(Error::unsupported(
                "mail parameters which are not a dictionary",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    Ok(Some(Mail {
        id: rows::int(&row, "id")?,
        phid: rows::text(&row, "phid")?,
        actor_phid: rows::opt_text(&row, "actorPHID")?,
        parameters,
        status: rows::text(&row, "status")?,
        message: rows::opt_text(&row, "message")?,
        related_phid: rows::opt_text(&row, "relatedPHID")?,
    }))
}

/// `save()` of a loaded mail: every column is written, `parameters` encoded
/// as `phutil_json_encode()` encodes the decoded array, `dateModified` set
/// to `now`.
pub async fn save(database: &Database, mail: &Mail, now: i64) -> Result<(), DbError> {
    let sql = format!(
        "UPDATE {} SET phid = ?, actorPHID = ?, parameters = ?, status = ?, message = ?, \
         relatedPHID = ?, dateModified = ? WHERE id = ?",
        database.table("metamta", "metamta_mail")
    );
    let parameters = encode_parameters(&mail.parameters);
    phab_db::sqlx::query(&sql)
        .bind(&mail.phid)
        .bind(&mail.actor_phid)
        .bind(parameters)
        .bind(&mail.status)
        .bind(&mail.message)
        .bind(&mail.related_phid)
        .bind(now)
        .bind(mail.id)
        .execute(database.pool())
        .await?;
    Ok(())
}

/// The `parameters` column PHP writes for these parameters.
pub fn encode_parameters(parameters: &Map<String, Value>) -> String {
    phab_core::php::json_encode(&phab_core::php::round_trip(&Value::Object(
        parameters.clone(),
    )))
}
