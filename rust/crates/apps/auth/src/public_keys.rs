//! `auth.querypublickeys` (`PhabricatorAuthQueryPublicKeysConduitAPIMethod`):
//! the active SSH keys (`PhabricatorAuthSSHKeyQuery`), newest first, with
//! a cursor pager.
//!
//! A key has the policies of the object it authenticates: users, whom
//! anyone can see, or Almanac devices. Keys of users who don't exist are
//! dropped, as PHP's `willFilterPage()` does. Left to PHP: pages with keys
//! of other objects (Almanac devices), empty lists (PHP's query fails to
//! build) and values of unusual types.

use std::fmt::Write as _;

use phab_conduit::pager::{self, Cursor, Direction, PageSize, Paging, PolicyAwareQuery};
use phab_conduit::search::{Clauses, IdOrder};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_db::rows;
use phab_db::sqlx::mysql::MySqlRow;
use serde_json::{Value, json};

/// The key types `PhabricatorAuthSSHPublicKey::newFromRawKey()` accepts.
const RECOGNIZED_KEYS: [&str; 7] = [
    "ssh-dsa",
    "ssh-dss",
    "ssh-rsa",
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
];

/// An `auth_sshkey` row.
#[derive(Debug, Clone)]
struct SshKey {
    id: i64,
    phid: String,
    object_phid: String,
    name: String,
    key_type: String,
    key_body: String,
    key_comment: String,
    is_trusted: bool,
}

impl SshKey {
    fn from_row(row: &MySqlRow) -> Result<Self, phab_db::DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: rows::text(row, "phid")?,
            object_phid: rows::text(row, "objectPHID")?,
            name: rows::text(row, "name")?,
            key_type: rows::text(row, "keyType")?,
            key_body: rows::text(row, "keyBody")?,
            key_comment: rows::text(row, "keyComment")?,
            is_trusted: rows::flag(row, "isTrusted")?,
        })
    }

    /// `getEntireKey()`.
    fn entire_key(&self) -> String {
        format!("{} {} {}", self.key_type, self.key_body, self.key_comment)
            .trim_matches(TRIM)
            .to_owned()
    }
}

/// PHP's `trim()` characters.
const TRIM: [char; 6] = [' ', '\t', '\n', '\r', '\0', '\x0b'];

/// PCRE's `\s`.
fn is_pcre_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// `preg_split('/\s+/', $text, 3)`.
fn split_three(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = text;
    while parts.len() < 2 {
        let Some(start) = rest.find(is_pcre_space) else {
            break;
        };
        let end = rest[start..]
            .find(|character: char| !is_pcre_space(character))
            .map_or(rest.len(), |offset| start + offset);
        parts.push(&rest[..start]);
        rest = &rest[end..];
    }
    parts.push(rest);
    parts
}

/// `PhabricatorAuthSSHPublicKey::newFromRawKey()`: the type and the hash
/// (`getHash()`) of a key, or PHP's exception message.
fn parse_key(entire_key: &str) -> Result<(String, String), MethodError> {
    let error = |message: String| pager::core_error(message);
    let entire_key = entire_key.trim_matches(TRIM);
    if entire_key.is_empty() {
        return Err(error("No public key was provided.".to_owned()));
    }
    let joined = entire_key.replace('\n', "");
    let mut parts = split_three(&joined);
    let lowered = entire_key.to_ascii_lowercase();
    let private = lowered.match_indices("private").any(|(index, _)| {
        lowered[index + "private".len()..]
            .trim_start_matches(is_pcre_space)
            .starts_with("key")
    });
    if private {
        return Err(error("Provide a public key, not a private key!".to_owned()));
    }
    match parts.len() {
        1 => {
            return Err(error(
                "Provided public key is not properly formatted.".to_owned(),
            ));
        }
        2 => parts.push(""),
        _ => {}
    }
    let (key_type, body) = (parts[0], parts[1]);
    if !RECOGNIZED_KEYS.contains(&key_type) {
        return Err(error(format!(
            "Public key type should be one of: {}",
            RECOGNIZED_KEYS.join(", ")
        )));
    }
    let body = body.trim_matches(TRIM).trim_end_matches('=');
    Ok((
        key_type.to_owned(),
        phab_core::hash::digest_for_index(body.as_bytes()),
    ))
}

/// An `optional list<...>` parameter, used when not `null`.
fn list(request: &ApiRequest<'_>, key: &str) -> Result<Option<Vec<Value>>, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if !items.is_empty() => Ok(Some(items.clone())),
        Some(Value::Array(_)) => Err(MethodError::Unsupported("empty lists (PHP's query fails)")),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// A list of strings.
fn strings(values: Vec<Value>) -> Result<Vec<String>, MethodError> {
    values
        .into_iter()
        .map(|value| match value {
            Value::String(text) => Ok(text),
            _ => Err(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        })
        .collect()
}

/// `PhabricatorAuthSSHKeyQuery` with `withIsActive(true)`.
struct Keys<'a> {
    request: &'a ApiRequest<'a>,
    clauses: Clauses,
}

impl PolicyAwareQuery for Keys<'_> {
    type Row = SshKey;

    fn query_class(&self) -> &'static str {
        "PhabricatorAuthSSHKeyQuery"
    }

    fn id(row: &SshKey) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<SshKey>, MethodError> {
        let database = &self.request.context.database;
        // A cursor's own query has none of the constraints.
        let mut clauses = match ids {
            Some(ids) => {
                let mut clauses = Clauses::default();
                clauses.in_list("k.id", ids.iter().map(|id| json!(id)));
                clauses
            }
            None => self.clauses.clone(),
        };
        let (comparison, order) = phab_conduit::search::id_paging(IdOrder::Newest, direction);
        if let Some(after) = after {
            clauses.condition(format!("k.id {comparison} ?"), [json!(after)]);
        }
        let select = format!("SELECT * FROM {} k", database.table("auth", "auth_sshkey"));
        let mut tail = format!(" ORDER BY k.id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(&select, &tail);
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(SshKey::from_row)
            .collect::<Result<_, _>>()?)
    }

    /// `willFilterPage()`: keys of users who exist, whom anyone can see.
    async fn filter(&self, rows: Vec<SshKey>) -> Result<Vec<SshKey>, MethodError> {
        if rows
            .iter()
            .any(|key| !key.object_phid.starts_with("PHID-USER-"))
        {
            return Err(MethodError::Unsupported("keys of objects other than users"));
        }
        let mut phids: Vec<&str> = rows.iter().map(|key| key.object_phid.as_str()).collect();
        phids.sort_unstable();
        phids.dedup();
        if phids.is_empty() {
            return Ok(rows);
        }
        let database = &self.request.context.database;
        let sql = format!(
            "SELECT phid FROM {} WHERE phid IN ({})",
            database.table("user", "user"),
            phab_db::placeholders(phids.len())
        );
        let users: Vec<String> = phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| rows::text(row, "phid"))
            .collect::<Result<_, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|key| users.contains(&key.object_phid))
            .collect())
    }
}

/// `ConduitAPIMethod::newPager()`: the limit, 100 by default, between 1
/// and 1000, and the cursors.
fn paging(request: &ApiRequest<'_>) -> Result<Paging, MethodError> {
    let size = match request.value("limit") {
        None | Some(Value::Null) => 100,
        Some(Value::Number(number)) if !number.is_f64() => {
            let limit = number
                .as_i64()
                .ok_or(MethodError::Unsupported("a huge limit"))?;
            usize::try_from(limit.clamp(1, 1000)).map_err(MethodError::internal)?
        }
        Some(_) => return Err(MethodError::Unsupported("an unusual limit")),
    };
    Ok(Paging {
        size: PageSize {
            shown: json!(size),
            value: size,
        },
        after: Cursor::parse(request.value("after"))?,
        before: Cursor::parse(request.value("before"))?,
    })
}

/// The constraints of the call.
fn clauses(request: &ApiRequest<'_>) -> Result<Clauses, MethodError> {
    let mut clauses = Clauses::default();
    if let Some(ids) = list(request, "ids")? {
        let ids = ids
            .into_iter()
            .map(|id| match id {
                Value::Number(number) if !number.is_f64() => Some(json!(number)),
                Value::String(text)
                    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
                {
                    Some(json!(text))
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(MethodError::Unsupported("IDs which are not integers"))?;
        clauses.in_list("k.id", ids);
    }
    for (key, column) in [("phids", "k.phid"), ("objectPHIDs", "k.objectPHID")] {
        if let Some(values) = list(request, key)? {
            clauses.in_list(column, strings(values)?.into_iter().map(Value::String));
        }
    }
    if let Some(keys) = list(request, "keys")? {
        let mut conditions = Vec::new();
        let mut binds = Vec::new();
        for key in strings(keys)? {
            let (key_type, hash) = parse_key(&key)?;
            conditions.push("(k.keyType = ? AND k.keyIndex = ?)");
            binds.push(json!(key_type));
            binds.push(json!(hash));
        }
        clauses.condition(format!("({})", conditions.join(" OR ")), binds);
    }
    clauses.condition("k.isActive = 1".to_owned(), []);
    Ok(clauses)
}

/// `auth.querypublickeys`.
pub struct QueryPublicKeys;

impl Method for QueryPublicKeys {
    fn name(&self) -> &'static str {
        "auth.querypublickeys"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "ids",
            "phids",
            "objectPHIDs",
            "keys",
            "before",
            "after",
            "limit",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let clauses = clauses(request)?;
            let paging = paging(request)?;
            let query = Keys { request, clauses };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|key| {
                    json!({
                        // getID() is the string PHP read from the row.
                        "id": key.id.to_string(),
                        "name": key.name,
                        "phid": key.phid,
                        "objectPHID": key.object_phid,
                        "isTrusted": key.is_trusted,
                        "key": key.entire_key(),
                    })
                })
                .collect();
            let cursor = |key: Option<SshKey>| key.map(|row| row.id.to_string());
            Ok(json!({
                "data": data,
                "cursor": {
                    "limit": paging.size.value,
                    "after": cursor(page.next),
                    "before": cursor(page.previous),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_preg_split() {
        assert_eq!(split_three("a b c d"), ["a", "b", "c d"]);
        assert_eq!(split_three("a \t b"), ["a", "b"]);
        assert_eq!(split_three("a"), ["a"]);
        assert_eq!(split_three("\x0ca b"), ["", "a", "b"]);
    }

    #[test]
    fn parses_keys_like_php() {
        let (key_type, hash) = parse_key(" ssh-ed25519 AAAAC3== me@host \n").unwrap();
        assert_eq!(key_type, "ssh-ed25519");
        assert_eq!(hash, phab_core::hash::digest_for_index(b"AAAAC3"));
        assert_eq!(
            parse_key("ssh-rsa"),
            Err(pager::core_error(
                "Provided public key is not properly formatted.".to_owned()
            ))
        );
        assert_eq!(
            parse_key("-----BEGIN RSA PRIVATE   KEY----- x"),
            Err(pager::core_error(
                "Provide a public key, not a private key!".to_owned()
            ))
        );
        assert_eq!(
            parse_key("   "),
            Err(pager::core_error("No public key was provided.".to_owned()))
        );
        assert!(parse_key("ssh-foo AAAA").is_err());
    }
}
