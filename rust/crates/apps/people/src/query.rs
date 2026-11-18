//! `user.query` (`UserQueryConduitAPIMethod`): users by name, email, real
//! name, PHID or ID, newest first, with their availability.
//!
//! Availability comes from the cache on the user row
//! (`getAvailabilityCache()`). When it expired, PHP rebuilds it from
//! calendar events: Rust computes it only for users invited to no event,
//! who are available, and leaves the others to PHP.

use std::fmt::Write;

use phab_auth::User;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::{Database, rows};
use serde_json::{Value, json};

use crate::whoami::{primary_email, user_information};
use crate::{APPLICATION, profile_image};

/// A `list<string>` parameter; falsy is no constraint.
fn string_list(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// A `list<uint>` parameter.
fn id_list(value: Option<&Value>) -> Result<Vec<i64>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => list
            .iter()
            .map(|id| match id {
                Value::Number(number) if !number.is_f64() => number.as_i64(),
                Value::String(text)
                    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    text.parse().ok()
                }
                _ => None,
            })
            .collect::<Option<_>>()
            .ok_or(MethodError::Unsupported("IDs which are not integers")),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// An `optional int` parameter: `default` when absent, 0 when falsy.
fn count(value: Option<&Value>, default: u64) -> Result<u64, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(value) if !php::is_truthy(value) => Ok(0),
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or(MethodError::Unsupported("an unusual limit or offset")),
        Some(Value::String(text)) if text.bytes().all(|b| b.is_ascii_digit()) => text
            .parse()
            .map_err(|_| MethodError::Unsupported("a huge limit or offset")),
        Some(_) => Err(MethodError::Unsupported("an unusual limit or offset")),
    }
}

/// The constraints of `user.query`.
#[derive(Debug, Default)]
struct Filters {
    usernames: Vec<String>,
    emails: Vec<String>,
    realnames: Vec<String>,
    phids: Vec<String>,
    ids: Vec<i64>,
}

/// The PHIDs of the users matching `filters`, newest first
/// (`PhabricatorPeopleQuery`, which every user may see).
async fn user_phids(
    database: &Database,
    filters: &Filters,
    limit: u64,
    offset: u64,
) -> Result<Vec<Phid>, MethodError> {
    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<Value> = Vec::new();
    let strings = |values: &[String]| -> Vec<Value> { values.iter().map(|v| json!(v)).collect() };
    for (column, values) in [
        ("userName", strings(&filters.usernames)),
        ("realName", strings(&filters.realnames)),
        ("phid", strings(&filters.phids)),
        ("id", filters.ids.iter().map(|id| json!(id)).collect()),
    ] {
        if !values.is_empty() {
            conditions.push(format!(
                "{column} IN ({})",
                phab_db::placeholders(values.len())
            ));
            binds.extend(values);
        }
    }
    // withEmails() joins the user's addresses.
    if !filters.emails.is_empty() {
        conditions.push(format!(
            "phid IN (SELECT userPHID FROM {} WHERE address IN ({}))",
            database.table("user", "user_email"),
            phab_db::placeholders(filters.emails.len())
        ));
        binds.extend(strings(&filters.emails));
    }
    let mut sql = format!("SELECT phid FROM {}", database.table("user", "user"));
    if !conditions.is_empty() {
        let _ = write!(sql, " WHERE {}", conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY id DESC");
    match (limit, offset) {
        (0, 0) => {}
        (0, offset) => {
            let _ = write!(sql, " LIMIT {offset}, 18446744073709551615");
        }
        (limit, offset) => {
            let _ = write!(sql, " LIMIT {offset}, {limit}");
        }
    }
    let mut query = phab_db::sqlx::query(&sql);
    for bind in &binds {
        query = match bind {
            Value::String(text) => query.bind(text.as_str()),
            other => query.bind(other.as_i64()),
        };
    }
    query
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| Ok(Phid::new(rows::text(row, "phid")?)))
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// `getAwayUntil()`: when the user is back, from a valid availability
/// cache, or `None` for available users; users whose cache must be
/// rebuilt from calendar events are left to PHP.
async fn away_until(
    database: &Database,
    user: &Phid,
    now: i64,
) -> Result<Option<Value>, MethodError> {
    let sql = format!(
        "SELECT availabilityCache, availabilityCacheTTL FROM {} WHERE phid = ?",
        database.table("user", "user")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_one(database.pool())
        .await?;
    let ttl = rows::opt_int(&row, "availabilityCacheTTL")?;
    if ttl.is_some_and(|ttl| ttl > now) {
        let cache = rows::opt_text(&row, "availabilityCache")?
            .and_then(|cache| serde_json::from_str::<Value>(&cache).ok());
        if let Some(cache) = cache {
            return Ok(match cache.get("until") {
                None | Some(Value::Null) => None,
                Some(until) if !php::is_truthy(until) => None,
                Some(until) => Some(until.clone()),
            });
        }
    }
    // rebuildAvailabilityCache(): without invitations, the user is
    // available.
    let sql = format!(
        "SELECT 1 FROM {} WHERE inviteePHID = ? LIMIT 1",
        database.table("calendar", "calendar_eventinvitee")
    );
    let invited = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_optional(database.pool())
        .await?;
    if invited.is_some() {
        return Err(MethodError::Unsupported(
            "availability from calendar events",
        ));
    }
    Ok(None)
}

/// `user.query`.
pub struct UserQuery;

impl Method for UserQuery {
    fn name(&self) -> &'static str {
        "user.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "usernames",
            "emails",
            "realnames",
            "phids",
            "ids",
            "offset",
            "limit",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let context = request.context;
            let database = &context.database;
            let filters = Filters {
                usernames: string_list(request.value("usernames"))?,
                emails: string_list(request.value("emails"))?,
                realnames: string_list(request.value("realnames"))?,
                phids: string_list(request.value("phids"))?,
                ids: id_list(request.value("ids"))?,
            };
            let phids = user_phids(
                database,
                &filters,
                count(request.value("limit"), 100)?,
                count(request.value("offset"), 0)?,
            )
            .await?;
            let users: Vec<User> = phab_auth::user::load_users_by_phid(database, &phids).await?;
            let mut results = Vec::with_capacity(phids.len());
            for phid in &phids {
                let Some(user) = users.iter().find(|user| &user.phid == phid) else {
                    continue;
                };
                let image = profile_image::cached_uri(database, &context.config, user)
                    .await?
                    .ok_or(MethodError::Unsupported(
                        "profile image cache needs a rebuild",
                    ))?;
                let email = primary_email(database, user).await?;
                let mut result = user_information(&context.config, user, &image, email.as_ref())?;
                if let Value::Object(map) = &mut result {
                    map.remove("primaryEmail");
                    if let Some(until) = away_until(database, &user.phid, request.now).await? {
                        map.insert("currentStatus".to_owned(), json!("away"));
                        map.insert("currentStatusUntil".to_owned(), until);
                    }
                }
                results.push(result);
            }
            Ok(Value::Array(results))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_like_php() {
        assert_eq!(count(None, 100), Ok(100));
        assert_eq!(count(Some(&json!(0)), 100), Ok(0));
        assert_eq!(count(Some(&json!("5")), 100), Ok(5));
        assert!(count(Some(&json!(-1)), 100).is_err());
    }
}
