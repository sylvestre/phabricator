//! The `.add`, `.remove` and `.set` transactions of Conduit's list fields,
//! and what their PHIDs need.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, PhidType};
use phab_db::Database;
use serde_json::Value;

/// What a transaction does to a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    Add,
    Remove,
    Set,
}

impl Operation {
    /// The operation of a transaction type like `reviewers.add`.
    pub(super) fn of(kind: &str) -> Self {
        match kind.rsplit_once('.').map(|(_, operation)| operation) {
            Some("add") => Self::Add,
            Some("remove") => Self::Remove,
            _ => Self::Set,
        }
    }
}

/// A list transaction of the request.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Wanted {
    pub(super) operation: Operation,
    pub(super) phids: Vec<String>,
}

/// Whether `text` is a PHID of one of `types` (`PHID-USER-`, ...), with a
/// plain random part.
pub(super) fn is_phid_of(text: &str, types: &[&str]) -> bool {
    types.iter().any(|prefix| {
        text.strip_prefix(prefix).is_some_and(|random| {
            !random.is_empty()
                && random
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    })
}

/// A transaction of type `kind` whose value lists PHIDs of `types`; names
/// and datasource functions are left to PHP.
pub(super) fn parse(kind: &str, value: &Value, types: &[&str]) -> Result<Wanted, MethodError> {
    let unsupported = || MethodError::Unsupported("list values other than PHIDs");
    let Value::Array(items) = value else {
        return Err(unsupported());
    };
    let phids = items
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|text| is_phid_of(text, types))
                .map(str::to_owned)
                .ok_or_else(unsupported)
        })
        .collect::<Result<_, _>>()?;
    Ok(Wanted {
        operation: Operation::of(kind),
        phids,
    })
}

/// `getPHIDList()` and `getEdgeTransactionNewValue()`: the old PHIDs a set
/// keeps, the set's, the added ones, less the removed ones.
pub(super) fn new_list(old: &[String], wanted: &Wanted) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut put = |phid: &String| {
        if !result.contains(phid) {
            result.push(phid.clone());
        }
    };
    let set = wanted.operation == Operation::Set;
    for phid in old {
        if !set || wanted.phids.contains(phid) {
            put(phid);
        }
    }
    if wanted.operation != Operation::Remove {
        wanted.phids.iter().for_each(&mut put);
    }
    if wanted.operation == Operation::Remove {
        result.retain(|phid| !wanted.phids.contains(phid));
    }
    result
}

/// The PHIDs of `from` which are not in `other`, in order.
pub(super) fn minus(from: &[String], other: &[String]) -> Vec<String> {
    from.iter()
        .filter(|phid| !other.contains(phid))
        .cloned()
        .collect()
}

/// The application database holding the edges of `phid` as a source.
pub(super) fn edge_database(phid: &str) -> Result<&'static str, MethodError> {
    match Phid::new(phid).phid_type() {
        PhidType::USER => Ok("user"),
        PhidType::PROJECT => Ok("project"),
        PhidType::REVISION => Ok("differential"),
        PhidType::COMMIT => Ok("repository"),
        _ => Err(MethodError::Unsupported("edges of other objects")),
    }
}

/// Whether every user exists.
async fn users_exist(database: &Database, users: &[&str]) -> Result<bool, MethodError> {
    let sql = format!(
        "SELECT phid FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(users.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), users.iter().copied())
        .fetch_all(database.pool())
        .await?
        .len();
    Ok(found == users.len())
}

/// Whether every one of `phids` is a user who exists or a project the
/// viewer can see.
pub(super) async fn all_valid(
    request: &ApiRequest<'_>,
    phids: &[&str],
) -> Result<bool, MethodError> {
    let context = request.context;
    let database = &context.database;
    let (users, projects): (Vec<&str>, Vec<&str>) = phids
        .iter()
        .partition(|phid| Phid::new(**phid).phid_type() == PhidType::USER);
    if !users.is_empty() && !users_exist(database, &users).await? {
        return Ok(false);
    }
    if projects.is_empty() {
        return Ok(true);
    }
    let wanted: Vec<Phid> = projects.iter().map(|phid| Phid::new(*phid)).collect();
    let loaded = phab_app_project::storage::load_by_phid(database, &wanted).await?;
    let (visible, _) = phab_app_project::policy::visible_projects(
        database,
        &context.config,
        &request.viewer,
        loaded,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    Ok(visible.len() == projects.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(phids: &[&str]) -> Vec<String> {
        phids.iter().map(|phid| (*phid).to_owned()).collect()
    }

    fn wanted(operation: Operation, phids: &[&str]) -> Wanted {
        Wanted {
            operation,
            phids: list(phids),
        }
    }

    #[test]
    fn new_lists_like_php() {
        let old = list(&["a", "b"]);
        assert_eq!(
            new_list(&old, &wanted(Operation::Add, &["c", "a"])),
            list(&["a", "b", "c"])
        );
        assert_eq!(
            new_list(&old, &wanted(Operation::Remove, &["a"])),
            list(&["b"])
        );
        assert_eq!(
            new_list(&old, &wanted(Operation::Set, &["c", "b"])),
            list(&["b", "c"])
        );
    }

    #[test]
    fn parses_phids() {
        let types = &["PHID-USER-", "PHID-PROJ-"];
        let parsed = parse(
            "subscribers.remove",
            &serde_json::json!(["PHID-USER-a"]),
            types,
        );
        assert_eq!(parsed.unwrap(), wanted(Operation::Remove, &["PHID-USER-a"]));
        assert!(parse("subscribers.add", &serde_json::json!(["alice"]), types).is_err());
        assert!(
            parse(
                "projects.add",
                &serde_json::json!(["PHID-USER-a"]),
                &["PHID-PROJ-"]
            )
            .is_err()
        );
    }
}
