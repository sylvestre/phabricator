//! What every `*.search` method shares
//! (`PhabricatorApplicationSearchEngine::buildConduitResponse()`): reading
//! constraints with the Conduit parameter types, the attachments asked
//! for, the builtin `newest` and `oldest` orders, and the response.
//!
//! The parameter types follow `ConduitParameterType`: `strict` is
//! `ConduitAPIRequest::getIsStrictlyTyped()`, false when parameters came
//! as form fields, where integers and booleans may be strings.

use serde_json::{Map, Value, json};

use crate::pager::{self, Paged, Paging};
use crate::{ApiRequest, MethodError};

/// `raiseValidationException()`.
fn invalid(key: &str, message: &str) -> MethodError {
    pager::core_error(format!("Error while reading \"{key}\": {message}"))
}

/// The `constraints` of a request, which must all be in `valid` (the keys
/// of `getValidConstraintKeys()`, aliases included).
#[derive(Debug, Default)]
pub struct Constraints {
    entries: Map<String, Value>,
}

impl Constraints {
    /// Reads `constraints`: a map (or an empty list, which PHP decodes the
    /// same way); an unknown key is an error.
    pub fn parse(value: Option<&Value>, valid: &[&str]) -> Result<Self, MethodError> {
        let entries: Vec<(String, &Value)> = match value {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, value)| (key.clone(), value))
                .collect(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
            Some(_) => return Err(MethodError::Unsupported("constraints which are not a map")),
        };
        if let Some((key, _)) = entries
            .iter()
            .find(|(key, _)| !valid.contains(&key.as_str()))
        {
            return Err(pager::core_error(format!(
                "Constraint \"{key}\" is not a valid constraint for this query."
            )));
        }
        Ok(Self {
            entries: entries
                .into_iter()
                .map(|(key, value)| (key, value.clone()))
                .collect(),
        })
    }

    /// The value of a constraint, by key or alias (the first given).
    pub fn get(&self, keys: &[&str]) -> Option<(&str, &Value)> {
        keys.iter()
            .find_map(|key| self.entries.get_key_value(*key))
            .map(|(key, value)| (key.as_str(), value))
    }

    /// Whether any of these constraints is given.
    pub fn has(&self, keys: &[&str]) -> bool {
        self.get(keys).is_some()
    }
}

/// `ConduitListParameterType`: a list, nonempty unless `allow_empty`.
pub fn list<'v>(
    key: &str,
    value: &'v Value,
    allow_empty: bool,
) -> Result<&'v [Value], MethodError> {
    match value {
        Value::Array(list) if list.is_empty() && !allow_empty => Err(invalid(
            key,
            "Expected a nonempty list, but value is an empty list.",
        )),
        Value::Array(list) => Ok(list),
        // PHP decodes {"0": ...} into a list.
        Value::Object(map)
            if map
                .keys()
                .all(|key| key.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Err(MethodError::Unsupported("constraint lists sent as objects"))
        }
        Value::Object(_) => Err(invalid(key, "Expected a list, but value is an object.")),
        _ => Err(invalid(key, "Expected a list, but value is not a list.")),
    }
}

/// `parseStringValue()`.
pub fn string(key: &str, value: &Value) -> Result<String, MethodError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(key, "Expected string, got something else."))
}

/// `parseIntValue()`.
pub fn int(key: &str, value: &Value, strict: bool) -> Result<i64, MethodError> {
    match value {
        Value::Number(number) if !number.is_f64() => number
            .as_i64()
            .ok_or(MethodError::Unsupported("huge integers")),
        Value::String(text)
            if !strict && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
        {
            text.parse().map_err(|_| invalid(key, "Integer overflow."))
        }
        _ => Err(invalid(key, "Expected integer, got something else.")),
    }
}

/// `parseBoolValue()`.
pub fn bool(key: &str, value: &Value, strict: bool) -> Result<bool, MethodError> {
    match value {
        Value::Bool(value) => Ok(*value),
        Value::String(text) if !strict => match text.to_lowercase().as_str() {
            "0" | "false" => Ok(false),
            "1" | "true" => Ok(true),
            _ => Err(invalid(
                key,
                "Expected boolean (true or false), got something else.",
            )),
        },
        _ => Err(invalid(
            key,
            "Expected boolean (true or false), got something else.",
        )),
    }
}

/// `ConduitStringListParameterType` and `ConduitPHIDListParameterType`.
pub fn string_list(
    key: &str,
    value: &Value,
    allow_empty: bool,
) -> Result<Vec<String>, MethodError> {
    list(key, value, allow_empty)?
        .iter()
        .enumerate()
        .map(|(index, item)| string(&format!("{key}[{index}]"), item))
        .collect()
}

/// `ConduitIntListParameterType`.
pub fn int_list(key: &str, value: &Value, strict: bool) -> Result<Vec<i64>, MethodError> {
    list(key, value, false)?
        .iter()
        .enumerate()
        .map(|(index, item)| int(&format!("{key}[{index}]"), item, strict))
        .collect()
}

/// PHIDs of one type as given, for the parameter types which also take
/// names and functions (`ConduitUserListParameterType`, `viewer()`):
/// those are resolved by PHP.
pub fn plain_phids(list: &[Value], prefix: &str) -> Result<Vec<String>, MethodError> {
    list.iter()
        .map(|phid| {
            phid.as_str()
                .filter(|phid| phid.starts_with(prefix))
                .map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(MethodError::Unsupported(
            "constraint values which are not PHIDs",
        ))
}

/// `ConduitEpochParameterType`: a positive integer.
pub fn epoch(key: &str, value: &Value, strict: bool) -> Result<i64, MethodError> {
    if !matches!(value, Value::Number(_) | Value::String(_)) {
        return Err(MethodError::Unsupported("unusual epochs"));
    }
    let epoch = int(key, value, strict)?;
    if epoch <= 0 {
        return Err(invalid(
            key,
            &format!("Epoch timestamp must be larger than 0, got {epoch}."),
        ));
    }
    Ok(epoch)
}

/// `queryKey`: saved and builtin queries are left to PHP.
pub fn check_query_key(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    match request.value("queryKey") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(key)) if key.is_empty() => Ok(()),
        Some(_) => Err(MethodError::Unsupported("saved or builtin queries")),
    }
}

/// The attachments asked for, in the order of the request:
/// `array_select_keys()` keeps the keys which are attachments, whatever
/// their value.
pub fn requested_attachments(
    value: Option<&Value>,
    known: &[&'static str],
) -> Result<Vec<&'static str>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) if list.is_empty() => Ok(Vec::new()),
        Some(Value::Object(map)) => Ok(map
            .keys()
            .filter_map(|key| known.iter().find(|known| **known == key))
            .copied()
            .collect()),
        Some(_) => Err(MethodError::Unsupported("attachments which are not a map")),
    }
}

/// The builtin orders every query has: `newest` (alias `created`), the
/// default, and `oldest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdOrder {
    /// By decreasing ID.
    Newest,
    /// By increasing ID.
    Oldest,
}

impl IdOrder {
    /// The `order` parameter. `supported` lists the query's builtin orders
    /// and aliases for PHP's error, in its order; the other orders it
    /// names, and order vectors, are left to PHP.
    pub fn parse(
        value: Option<&Value>,
        query_class: &str,
        supported: &[&str],
    ) -> Result<Self, MethodError> {
        let key = match value {
            None | Some(Value::Null) => return Ok(Self::Newest),
            Some(Value::String(key)) => key.as_str(),
            Some(_) => return Err(MethodError::Unsupported("order vectors")),
        };
        match key {
            "newest" | "created" => Ok(Self::Newest),
            "oldest" => Ok(Self::Oldest),
            other if supported.contains(&other) => {
                Err(MethodError::Unsupported("an order not ported"))
            }
            other => Err(pager::core_error(format!(
                "Query \"{query_class}\" does not support a builtin order \"{other}\". \
                 Supported orders are: {}.",
                supported.join(", ")
            ))),
        }
    }

    /// Whether going forward means decreasing IDs.
    pub fn descending(self) -> bool {
        self == Self::Newest
    }
}

/// The `ORDER BY` and the cursor condition of a query in ID order:
/// `(comparison, direction)`, like `id < ?` and `DESC`.
pub fn id_paging(order: IdOrder, direction: pager::Direction) -> (&'static str, &'static str) {
    if order.descending() == (direction == pager::Direction::Forward) {
        ("<", "DESC")
    } else {
        (">", "ASC")
    }
}

/// The joins and conditions of a query, with the values they bind, built
/// up like `buildJoinClauseParts()` and `buildWhereClauseParts()`.
#[derive(Debug, Default)]
pub struct Clauses {
    joins: Vec<String>,
    join_binds: Vec<Value>,
    conditions: Vec<String>,
    binds: Vec<Value>,
}

impl Clauses {
    /// Adds a join and the values its placeholders bind.
    pub fn join(&mut self, sql: String, binds: impl IntoIterator<Item = Value>) {
        self.joins.push(sql);
        self.join_binds.extend(binds);
    }

    /// Adds a condition and the values its placeholders bind.
    pub fn condition(&mut self, sql: impl Into<String>, binds: impl IntoIterator<Item = Value>) {
        self.conditions.push(sql.into());
        self.binds.extend(binds);
    }

    /// Adds `column IN (...)`.
    pub fn in_list(&mut self, column: &str, values: impl IntoIterator<Item = Value>) {
        let values: Vec<Value> = values.into_iter().collect();
        self.condition(
            format!("{column} IN ({})", phab_db::placeholders(values.len())),
            values,
        );
    }

    /// Whether the query joins other tables, and so must group its rows.
    pub fn has_joins(&self) -> bool {
        !self.joins.is_empty()
    }

    /// The query: `select` (`SELECT ... FROM ...`), the joins, the
    /// conditions (each in parentheses, like `%LA`), then `tail` (`GROUP
    /// BY`, `ORDER BY`, `LIMIT`).
    pub fn sql(&self, select: &str, tail: &str) -> String {
        let mut sql = select.to_owned();
        for join in &self.joins {
            sql.push(' ');
            sql.push_str(join);
        }
        if !self.conditions.is_empty() {
            sql.push_str(" WHERE ");
            let conditions: Vec<String> = self
                .conditions
                .iter()
                .map(|condition| format!("({condition})"))
                .collect();
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(tail);
        sql
    }

    /// Binds the values of the joins, then of the conditions, to `query`.
    pub fn bind<'q>(
        &'q self,
        mut query: phab_db::sqlx::query::Query<
            'q,
            phab_db::sqlx::MySql,
            phab_db::sqlx::mysql::MySqlArguments,
        >,
    ) -> phab_db::sqlx::query::Query<'q, phab_db::sqlx::MySql, phab_db::sqlx::mysql::MySqlArguments>
    {
        for bind in self.join_binds.iter().chain(&self.binds) {
            query = match bind {
                Value::String(text) => query.bind(text.as_str()),
                other => query.bind(other.as_i64()),
            };
        }
        query
    }
}

/// JSON values of strings, for [`Clauses`].
pub fn strings(values: &[String]) -> Vec<Value> {
    values.iter().map(|value| json!(value)).collect()
}

/// The response: `data`, empty `maps`, no `queryKey`, and the cursor of the
/// page, whose `after` and `before` are the IDs of `page`'s neighbours.
pub fn response<T>(
    request: &ApiRequest<'_>,
    data: &[Value],
    paging: &Paging,
    page: &Paged<T>,
    id: impl Fn(&T) -> String,
) -> Value {
    json!({
        "data": data,
        "maps": {},
        "query": {"queryKey": null},
        "cursor": {
            "limit": paging.size.shown,
            "after": page.next.as_ref().map(&id),
            "before": page.previous.as_ref().map(&id),
            "order": request.value("order").cloned().unwrap_or(Value::Null),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(message: &str) -> MethodError {
        pager::core_error(message.to_owned())
    }

    #[test]
    fn constraints_must_be_known() {
        let value = json!({"ids": [1], "nope": 1});
        assert_eq!(
            Constraints::parse(Some(&value), &["ids"]).unwrap_err(),
            error("Constraint \"nope\" is not a valid constraint for this query.")
        );
        let value = json!(["x"]);
        assert_eq!(
            Constraints::parse(Some(&value), &["ids"]).unwrap_err(),
            error("Constraint \"0\" is not a valid constraint for this query.")
        );
        let value = json!({"username": ["a"]});
        let constraints = Constraints::parse(Some(&value), &["usernames", "username"]).unwrap();
        assert_eq!(
            constraints
                .get(&["usernames", "username"])
                .map(|(key, _)| key),
            Some("username")
        );
    }

    #[test]
    fn values_parse_like_php() {
        assert_eq!(int("k", &json!("12"), false), Ok(12));
        assert_eq!(
            int("k", &json!("12"), true),
            Err(error(
                "Error while reading \"k\": Expected integer, got something else."
            ))
        );
        assert_eq!(bool("k", &json!("TRUE"), false), Ok(true));
        assert_eq!(
            bool("k", &json!(1), false),
            Err(error(
                "Error while reading \"k\": Expected boolean (true or false), got something else."
            ))
        );
        assert_eq!(
            string_list("k", &json!(["a", 1]), false),
            Err(error(
                "Error while reading \"k[1]\": Expected string, got something else."
            ))
        );
        assert_eq!(
            string_list("k", &json!([]), false),
            Err(error(
                "Error while reading \"k\": Expected a nonempty list, but value is an empty list."
            ))
        );
        assert_eq!(string_list("k", &json!([]), true), Ok(vec![]));
        assert_eq!(
            epoch("k", &json!(0), true),
            Err(error(
                "Error while reading \"k\": Epoch timestamp must be larger than 0, got 0."
            ))
        );
    }

    #[test]
    fn id_orders() {
        let supported = ["newest", "created", "oldest", "relevance"];
        assert_eq!(IdOrder::parse(None, "Q", &supported), Ok(IdOrder::Newest));
        assert_eq!(
            IdOrder::parse(Some(&json!("oldest")), "Q", &supported),
            Ok(IdOrder::Oldest)
        );
        assert!(matches!(
            IdOrder::parse(Some(&json!("relevance")), "Q", &supported),
            Err(MethodError::Unsupported(_))
        ));
        assert_eq!(
            IdOrder::parse(Some(&json!("x")), "Q", &supported),
            Err(error(
                "Query \"Q\" does not support a builtin order \"x\". Supported orders are: \
                 newest, created, oldest, relevance."
            ))
        );
    }
}
