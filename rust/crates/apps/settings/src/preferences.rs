//! `PhabricatorUserPreferences`: the stored settings of a user, and the
//! global defaults.

use phab_core::Phid;
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value};

use crate::Error;

/// `PhabricatorUserPreferences::BUILTIN_GLOBAL_DEFAULT`.
pub const BUILTIN_GLOBAL: &str = "global";

/// The stored settings of a user, as PHP decodes them (`json_decode($value,
/// true)`): keys in their stored order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Preferences {
    /// The row, if the user has one; PHP makes up an empty object
    /// otherwise (`needSyntheticPreferences()`).
    pub row: Option<Row>,
    /// The settings.
    pub values: Map<String, Value>,
}

/// The identity of a stored preferences row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `id`.
    pub id: i64,
    /// `phid`.
    pub phid: String,
}

impl Preferences {
    /// `getPreference()`: the stored value, `null` when there is none.
    pub fn get(&self, key: &str) -> &Value {
        self.values.get(key).unwrap_or(&Value::Null)
    }
}

/// The settings of a decoded `preferences` column. PHP keeps lists as
/// arrays keyed `0..n`; anything else isn't an array PHP could edit.
pub(crate) fn decode(value: Value) -> Result<Map<String, Value>, Error> {
    match value {
        Value::Object(map) => Ok(map),
        Value::Array(list) => Ok(list
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value))
            .collect()),
        _ => Err(Error::Unsupported("preferences which are not an array")),
    }
}

/// Loads the preferences of `user`, empty when they have none.
pub async fn load(database: &Database, user: &Phid) -> Result<Preferences, Error> {
    let sql = format!(
        "SELECT id, phid, preferences FROM {} WHERE userPHID = ?",
        database.table("user", "user_preferences")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(Preferences::default());
    };
    Ok(Preferences {
        row: Some(Row {
            id: rows::int(&row, "id")?,
            phid: rows::text(&row, "phid")?,
        }),
        values: decode(rows::json(&row, "preferences")?)?,
    })
}

/// The global default settings, if an administrator created them.
pub async fn load_global(database: &Database) -> Result<Option<Map<String, Value>>, Error> {
    let sql = format!(
        "SELECT preferences FROM {} WHERE builtinKey = ?",
        database.table("user", "user_preferences")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(BUILTIN_GLOBAL)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?;
    row.map(|row| decode(rows::json(&row, "preferences")?))
        .transpose()
}

/// `$a === $b` for decoded JSON values: arrays are identical when they
/// have the same pairs in the same order.
pub fn identical(left: &Value, right: &Value) -> bool {
    fn pairs(value: &Value) -> Option<Vec<(String, &Value)>> {
        match value {
            Value::Array(list) => Some(
                list.iter()
                    .enumerate()
                    .map(|(index, value)| (index.to_string(), value))
                    .collect(),
            ),
            Value::Object(map) => Some(
                map.iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect(),
            ),
            _ => None,
        }
    }
    match (pairs(left), pairs(right)) {
        (Some(left), Some(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(&right)
                    .all(|((left_key, left), (right_key, right))| {
                        left_key == right_key && identical(left, right)
                    })
        }
        (None, None) => left == right,
        _ => false,
    }
}

/// `(int)$value`.
pub fn int_cast(value: &Value) -> i64 {
    match value {
        Value::Null | Value::Bool(false) => 0,
        Value::Bool(true) => 1,
        #[expect(clippy::cast_possible_truncation, reason = "PHP truncates")]
        Value::Number(number) => number
            .as_i64()
            .unwrap_or_else(|| number.as_f64().unwrap_or_default().trunc() as i64),
        Value::String(text) => int_cast_str(text),
        Value::Array(list) => i64::from(!list.is_empty()),
        Value::Object(map) => i64::from(!map.is_empty()),
    }
}

/// `(int)$text`: the leading number, after leading whitespace.
pub fn int_cast_str(text: &str) -> i64 {
    let text = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let bytes = text.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits_start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let mut float_end = end;
    if bytes.get(float_end) == Some(&b'.') {
        float_end += 1;
        while bytes.get(float_end).is_some_and(u8::is_ascii_digit) {
            float_end += 1;
        }
    }
    let has_digits = text[digits_start..float_end]
        .bytes()
        .any(|byte| byte.is_ascii_digit());
    if !has_digits {
        return 0;
    }
    if matches!(bytes.get(float_end), Some(b'e' | b'E')) {
        let mut exponent_end = float_end + 1;
        if matches!(bytes.get(exponent_end), Some(b'+' | b'-')) {
            exponent_end += 1;
        }
        let exponent_digits = exponent_end;
        while bytes.get(exponent_end).is_some_and(u8::is_ascii_digit) {
            exponent_end += 1;
        }
        if exponent_end > exponent_digits {
            float_end = exponent_end;
        }
    }
    if float_end > end {
        // A float string: PHP converts it, then truncates.
        #[expect(clippy::cast_possible_truncation, reason = "PHP truncates")]
        return text[..float_end]
            .parse::<f64>()
            .map_or(0, |float| float.trunc() as i64);
    }
    text[..end].parse::<i64>().unwrap_or_else(|_| {
        if text.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[test]
    fn identical_arrays_have_the_same_pairs_in_order() {
        assert!(identical(
            &json!({"a": 1, "b": 2}),
            &json!({"a": 1, "b": 2})
        ));
        assert!(!identical(
            &json!({"a": 1, "b": 2}),
            &json!({"b": 2, "a": 1})
        ));
        assert!(!identical(&json!({"a": 1}), &json!({"a": "1"})));
        assert!(!identical(&json!(1), &json!(1.0)));
        assert!(identical(&json!([]), &json!({})));
        assert!(identical(&json!(["x"]), &json!({"0": "x"})));
        assert!(!identical(&Value::Null, &json!("")));
    }

    #[rstest]
    #[case("2", 2)]
    #[case(" 1", 1)]
    #[case("12abc", 12)]
    #[case("abc", 0)]
    #[case("", 0)]
    #[case("-3", -3)]
    #[case("1e3", 1000)]
    #[case("2.9", 2)]
    #[case("1e", 1)]
    fn strings_are_cast_like_php(#[case] text: &str, #[case] expected: i64) {
        assert_eq!(int_cast_str(text), expected);
    }
}
