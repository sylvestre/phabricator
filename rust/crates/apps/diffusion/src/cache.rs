//! Reads of `PhabricatorKeyValueDatabaseCache` (`cache_general`), the
//! database level of PHP's immutable and repository graph caches. Rust
//! reads entries PHP wrote; it writes none.

use std::io::Read as _;

use phab_conduit::{ApiRequest, MethodError};
use phab_db::rows;
use phab_db::sqlx::Row as _;
use serde_json::Value;

/// `PhabricatorCaches::getNamespace()` as a key prefix, "phabricator:" by
/// default.
fn namespace(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    Ok(
        match request.context.config.value("phabricator.cache-namespace") {
            None => "phabricator:".to_owned(),
            Some(Value::Null) => String::new(),
            Some(Value::String(namespace)) if namespace.is_empty() => String::new(),
            Some(Value::String(namespace)) => format!("{namespace}:"),
            Some(_) => return Err(MethodError::Unsupported("an unusual cache namespace")),
        },
    )
}

/// `readCacheData()`: the data of each of `keys` (without the namespace)
/// which has a live entry, in the order of `keys`. Entries which don't
/// inflate are misses, as in PHP.
pub async fn read(
    request: &ApiRequest<'_>,
    keys: &[String],
) -> Result<Vec<Option<Vec<u8>>>, MethodError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let namespace = namespace(request)?;
    let database = &request.context.database;
    let sql = format!(
        "SELECT cacheKey, cacheFormat, cacheData, cacheExpires FROM {} WHERE cacheKeyHash IN ({})",
        database.table("cache", "cache_general"),
        phab_db::placeholders(keys.len())
    );
    let full_keys: Vec<String> = keys.iter().map(|key| format!("{namespace}{key}")).collect();
    let found = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        full_keys
            .iter()
            .map(|key| phab_core::hash::digest_for_index(key.as_bytes())),
    )
    .fetch_all(database.pool())
    .await?;
    let mut cached = Vec::with_capacity(keys.len());
    for full_key in &full_keys {
        let Some(row) = found
            .iter()
            .rev()
            .find(|row| rows::text(row, "cacheKey").is_ok_and(|key| key == *full_key))
        else {
            cached.push(None);
            continue;
        };
        let expires = rows::opt_int(row, "cacheExpires")?.unwrap_or(0);
        if expires != 0 && expires < request.now {
            cached.push(None);
            continue;
        }
        let data: Vec<u8> = row
            .try_get::<Vec<u8>, _>("cacheData")
            .map_err(MethodError::internal)?;
        cached.push(match rows::text(row, "cacheFormat")?.as_str() {
            "raw" => Some(data),
            "deflate" => {
                let mut inflated = Vec::new();
                flate2::read::DeflateDecoder::new(data.as_slice())
                    .read_to_end(&mut inflated)
                    .is_ok()
                    .then_some(inflated)
            }
            _ => None,
        });
    }
    Ok(cached)
}
