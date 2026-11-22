//! `flag.edit` (`FlagEditConduitAPIMethod`) and `flag.delete`
//! (`FlagDeleteConduitAPIMethod`): flags are plain `flag` rows, which
//! Lisk saves and deletes without transactions.
//!
//! Ported: every parameter. Left to PHP: flags of objects whose handles
//! are not ported or which the viewer can't see (PHP shows a placeholder
//! handle), and values Lisk would write in ways MySQL may refuse or
//! convert (colors which are not unsigned integers, notes which are not
//! strings, `null`s).

use phab_app_phid::handles::{self, Handle};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use serde_json::{Value, json};

use crate::query::{Flag, color_name, production_uri};

/// `PhabricatorFlagColor::COLOR_BLUE`, the color of new flags.
const COLOR_BLUE: i64 = 4;

/// The acting user.
fn actor(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    request
        .viewer
        .phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned())
        .ok_or(MethodError::Unsupported("flags of the omnipotent user"))
}

/// `phid_get_type()`: the type of a PHID, `????` for anything else.
fn phid_type(phid: &str) -> String {
    phid.strip_prefix("PHID-")
        .and_then(|rest| {
            let kind = rest.get(..4)?;
            (rest.as_bytes().get(4) == Some(&b'-') && !kind.contains('-')).then_some(kind)
        })
        .unwrap_or("????")
        .to_owned()
}

/// A color Lisk stores as is in the `uint32` column: an integer, or the
/// digits of one, as PHP got it. Returns the value and the color.
fn color(value: &Value) -> Result<(Value, i64), MethodError> {
    let unsupported = || MethodError::Unsupported("colors which are not unsigned integers");
    let color = match value {
        Value::Number(number) => number.as_u64(),
        // `"07"` stores 7, yet `getColorName("07")` is "Unknown".
        Value::String(text)
            if !text.is_empty()
                && text.bytes().all(|byte| byte.is_ascii_digit())
                && (text == "0" || !text.starts_with('0')) =>
        {
            text.parse().ok()
        }
        _ => None,
    }
    .filter(|color| u32::try_from(*color).is_ok())
    .ok_or_else(unsupported)?;
    Ok((
        value.clone(),
        i64::try_from(color).map_err(|_| unsupported())?,
    ))
}

/// A note: a string.
fn note(value: &Value) -> Result<String, MethodError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or(MethodError::Unsupported("notes which are not strings"))
}

/// The flag of `owner` on `object`, if any (`loadOneWhere()`).
async fn load_by_object(
    request: &ApiRequest<'_>,
    object: &str,
    owner: &str,
) -> Result<Option<Flag>, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT * FROM {} WHERE objectPHID = ? AND ownerPHID = ?",
        database.table("flag", "flag")
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(object)
        .bind(owner)
        .fetch_all(database.pool())
        .await?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => Ok(Some(Flag::from_row(row)?)),
        // `loadOneWhere()` raises an exception.
        _ => Err(MethodError::Unsupported("several flags of one object")),
    }
}

/// `attachHandleToFlag()`: the handle of the flagged object, which must
/// be one the viewer can see.
async fn handle(request: &ApiRequest<'_>, object: &str) -> Result<Handle, MethodError> {
    let phid = Phid::new(object);
    if !handles::is_supported(&phid) {
        return Err(MethodError::Unsupported(
            "flags of objects whose handles aren't ported",
        ));
    }
    handles::load(request, std::slice::from_ref(&phid))
        .await?
        .remove(&phid)
        .ok_or(MethodError::Unsupported(
            "flags of objects the viewer can't see",
        ))
}

/// `buildFlagInfoDictionary()` of a flag in memory, whose values keep the
/// types PHP has: strings read from MySQL, the request's values, integers
/// Lisk set.
struct Dictionary {
    id: Value,
    owner: String,
    kind: String,
    object: String,
    reason: String,
    color: (Value, i64),
    note: Value,
    created: Value,
    modified: Value,
}

impl Dictionary {
    /// A flag as loaded: every value is a string.
    fn loaded(flag: &Flag) -> Self {
        Self {
            id: json!(flag.id.to_string()),
            owner: flag.owner_phid.clone(),
            kind: flag.kind.clone(),
            object: flag.object_phid.clone(),
            reason: flag.reason_phid.clone(),
            color: (json!(flag.color.to_string()), flag.color),
            note: json!(flag.note),
            created: json!(flag.date_created.to_string()),
            modified: json!(flag.date_modified.to_string()),
        }
    }

    fn to_json(&self, handle: &Handle, base_uri: &str) -> Value {
        json!({
            "id": self.id,
            "ownerPHID": self.owner,
            "type": self.kind,
            "objectPHID": self.object,
            "reasonPHID": self.reason,
            "color": self.color.0,
            "colorName": color_name(self.color.1),
            "note": self.note,
            "handle": {
                "uri": format!("{}{}", base_uri.trim_end_matches('/'), handle.uri),
                "name": handle.name,
                "fullname": handle.full_name,
            },
            "dateCreated": self.created,
            "dateModified": self.modified,
        })
    }
}

/// `flag.edit`.
pub struct FlagEdit;

impl FlagEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let database = &request.context.database;
        let owner = actor(request)?;
        let Some(Value::String(object)) = request.value("objectPHID") else {
            return Err(MethodError::Unsupported(
                "object PHIDs which are not strings",
            ));
        };
        let existing = load_by_object(request, object, &owner).await?;
        // `isset()`: a null parameter is no parameter.
        let given = |key| request.value(key).filter(|value| !value.is_null());
        let handle = handle(request, object).await?;
        let base_uri = production_uri(request)?;
        let now = request.now;
        let table = database.table("flag", "flag");
        let (mut dictionary, new) = if let Some(flag) = existing {
            let mut dictionary = Dictionary::loaded(&flag);
            if let Some(value) = given("color") {
                dictionary.color = color(value)?;
            }
            if let Some(value) = given("note") {
                dictionary.note = json!(note(value)?);
            }
            // Lisk saves every column; only these may change.
            phab_db::sqlx::query(&format!(
                "UPDATE {table} SET color = ?, note = ?, dateModified = ? WHERE id = ?"
            ))
            .bind(dictionary.color.1)
            .bind(dictionary.note.as_str())
            .bind(now)
            .bind(flag.id)
            .execute(database.pool())
            .await?;
            (dictionary, false)
        } else {
            // `getValue($key, $default)`: an explicit null stays null, which
            // the columns refuse.
            let color = match request.value("color") {
                None => (json!(COLOR_BLUE), COLOR_BLUE),
                Some(Value::Null) => return Err(MethodError::Unsupported("null colors")),
                Some(value) => color(value)?,
            };
            let note = match request.value("note") {
                None => String::new(),
                Some(Value::Null) => return Err(MethodError::Unsupported("null notes")),
                Some(value) => note(value)?,
            };
            let kind = phid_type(object);
            let id = phab_db::sqlx::query(&format!(
                "INSERT INTO {table} (ownerPHID, type, objectPHID, reasonPHID, color, note, \
                 dateCreated, dateModified) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            ))
            .bind(&owner)
            .bind(&kind)
            .bind(object)
            .bind(&owner)
            .bind(color.1)
            .bind(&note)
            .bind(now)
            .bind(now)
            .execute(database.pool())
            .await?
            .last_insert_id();
            let dictionary = Dictionary {
                id: json!(id),
                owner: owner.clone(),
                kind,
                object: object.clone(),
                reason: owner.clone(),
                color,
                note: json!(note),
                created: json!(now),
                modified: json!(now),
            };
            (dictionary, true)
        };
        dictionary.modified = json!(now);
        let mut result = dictionary.to_json(&handle, &base_uri);
        result["new"] = json!(new);
        Ok(result)
    }
}

impl Method for FlagEdit {
    fn name(&self) -> &'static str {
        "flag.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["objectPHID", "color", "note"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

/// `ERR_NOT_FOUND` of `flag.delete`.
const ERR_NOT_FOUND: &str = "ERR_NOT_FOUND";
/// `ERR_WRONG_USER` of `flag.delete`.
const ERR_WRONG_USER: &str = "ERR_WRONG_USER";
/// `ERR_NEED_PARAM` of `flag.delete`.
const ERR_NEED_PARAM: &str = "ERR_NEED_PARAM";

fn conduit_error(code: &str) -> MethodError {
    MethodError::Conduit {
        code: code.to_owned(),
        info: None,
    }
}

/// `flag.delete`.
pub struct FlagDelete;

impl FlagDelete {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let database = &request.context.database;
        let owner = actor(request)?;
        let id = request.value("id").filter(|value| php::is_truthy(value));
        let object = request
            .value("objectPHID")
            .filter(|value| php::is_truthy(value));
        let flag = if let Some(id) = id {
            // `LiskDAO::load()`: an integer or its digits.
            let id: u64 = match id {
                Value::Number(number) => number.as_u64(),
                Value::String(text) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
                    text.parse().ok()
                }
                _ => None,
            }
            .ok_or(MethodError::Unsupported("flag IDs which are not integers"))?;
            let sql = format!(
                "SELECT * FROM {} WHERE id = ?",
                database.table("flag", "flag")
            );
            let row = phab_db::sqlx::query(&sql)
                .bind(id)
                .fetch_optional(database.pool())
                .await?
                .ok_or_else(|| conduit_error(ERR_NOT_FOUND))?;
            let flag = Flag::from_row(&row)?;
            if flag.owner_phid != owner {
                return Err(conduit_error(ERR_WRONG_USER));
            }
            flag
        } else if let Some(object) = object {
            let object = object.as_str().ok_or(MethodError::Unsupported(
                "object PHIDs which are not strings",
            ))?;
            match load_by_object(request, object, &owner).await? {
                Some(flag) => flag,
                None => return Ok(Value::Null),
            }
        } else {
            return Err(conduit_error(ERR_NEED_PARAM));
        };
        let handle = handle(request, &flag.object_phid).await?;
        let result = Dictionary::loaded(&flag).to_json(&handle, &production_uri(request)?);
        phab_db::sqlx::query(&format!(
            "DELETE FROM {} WHERE id = ?",
            database.table("flag", "flag")
        ))
        .bind(flag.id)
        .execute(database.pool())
        .await?;
        Ok(result)
    }
}

impl Method for FlagDelete {
    fn name(&self) -> &'static str {
        "flag.delete"
    }

    fn params(&self) -> &'static [&'static str] {
        &["id", "objectPHID"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[
            (ERR_NOT_FOUND, "Bad flag ID."),
            (ERR_WRONG_USER, "You are not the creator of this flag."),
            (ERR_NEED_PARAM, "Must pass an id or an objectPHID."),
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
    fn types_phids_like_php() {
        assert_eq!(phid_type("PHID-DREV-abc"), "DREV");
        assert_eq!(phid_type("PHID-XACT-DREV-abc"), "XACT");
        assert_eq!(phid_type("PHID-AB-c"), "????");
        assert_eq!(phid_type("D12"), "????");
    }

    #[test]
    fn keeps_colors_as_given() {
        assert_eq!(color(&json!(3)).unwrap(), (json!(3), 3));
        assert_eq!(color(&json!("0")).unwrap(), (json!("0"), 0));
        assert!(color(&json!("07")).is_err());
        assert!(color(&json!(-1)).is_err());
        assert!(color(&json!("red")).is_err());
        assert!(color(&json!(true)).is_err());
    }
}
