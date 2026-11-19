//! `DifferentialRevisionReviewersTransaction`: the `reviewers.add`,
//! `reviewers.remove` and `reviewers.set` transactions of Conduit.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, php};
use phab_db::Database;
use serde_json::{Value, json};

use super::lists::{self, Operation};
use crate::search::ReviewerRow;
use crate::storage::{Reviewer, Revision};

/// A reviewer transaction of the request: its reviewers, with the status
/// each asks for (`added`, or `blocking` for `blocking(...)`).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Wanted {
    pub(super) operation: Operation,
    pub(super) tokens: Vec<(String, String)>,
}

/// Reviewers and their statuses, in the order of a PHP array.
pub(super) type Map = Vec<(String, String)>;

/// A user or project PHID, or `blocking()` of one; other tokens (names,
/// other functions) are left to PHP's datasource.
fn token(text: &str) -> Option<(String, String)> {
    let (phid, status) = match text
        .strip_prefix("blocking(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(inner) => (inner, "blocking"),
        None => (text, "added"),
    };
    lists::is_phid_of(phid, &["PHID-USER-", "PHID-PROJ-"])
        .then(|| (phid.to_owned(), status.to_owned()))
}

/// The reviewer transaction of type `kind` with `value`.
pub(super) fn parse(kind: &str, value: &Value) -> Result<Wanted, MethodError> {
    let operation = Operation::of(kind);
    let unsupported = || MethodError::Unsupported("reviewers other than user and project PHIDs");
    let Value::Array(items) = value else {
        return Err(unsupported());
    };
    let tokens = items
        .iter()
        .map(|item| item.as_str().and_then(token).ok_or_else(unsupported))
        .collect::<Result<_, _>>()?;
    Ok(Wanted { operation, tokens })
}

fn status_of<'a>(map: &'a Map, phid: &str) -> Option<&'a str> {
    map.iter()
        .find(|(known, _)| known == phid)
        .map(|(_, status)| status.as_str())
}

/// `$map[$phid] = $status`: an existing key keeps its place.
fn put(map: &mut Map, phid: &str, status: &str) {
    match map.iter_mut().find(|(known, _)| known == phid) {
        Some(entry) => status.clone_into(&mut entry.1),
        None => map.push((phid.to_owned(), status.to_owned())),
    }
}

/// `generateNewValue()`: reviewers who stay keep their status (accepts,
/// rejections) unless they become blocking or stop being.
pub(super) fn new_value(old: &Map, wanted: &Wanted) -> Map {
    let mut reviewers = old.clone();
    let mut add: Map = Vec::new();
    match wanted.operation {
        Operation::Remove => {
            reviewers.retain(|(phid, _)| !wanted.tokens.iter().any(|(gone, _)| gone == phid));
        }
        Operation::Add => {
            for (phid, status) in &wanted.tokens {
                put(&mut add, phid, status);
            }
        }
        // Setting adds to an empty list.
        Operation::Set => {
            for (phid, status) in &wanted.tokens {
                put(&mut add, phid, status);
            }
            reviewers.clear();
        }
    }
    for (phid, status) in &add {
        if let Some(old_status) = status_of(old, phid)
            && (old_status == "blocking") == (status == "blocking")
        {
            put(&mut reviewers, phid, old_status);
            continue;
        }
        put(&mut reviewers, phid, status);
    }
    reviewers
}

/// `getTransactionHasEffect()`: a new order alone is no change.
fn has_effect(old: &Map, new: &Map) -> bool {
    let (mut old, mut new) = (old.clone(), new.clone());
    old.sort();
    new.sort();
    old != new
}

/// A reviewer map as its transaction stores it: an empty one is `[]`.
pub(super) fn encode(map: &Map) -> String {
    if map.is_empty() {
        return "[]".to_owned();
    }
    php::json_encode(&Value::Object(
        map.iter()
            .map(|(phid, status)| (phid.clone(), json!(status)))
            .collect(),
    ))
}

/// The planned transaction: its PHID, and the reviewers before and after.
pub(super) struct Planned {
    pub(super) phid: String,
    pub(super) old: Map,
    pub(super) new: Map,
}

/// `validateTransactions()` of the reviewers the edit adds: users who
/// exist, projects the actor can see, never the author (unless
/// `differential.allow-self-accept`). PHP explains refusals.
async fn check_added(
    request: &ApiRequest<'_>,
    revision: &Revision,
    added: &[&str],
) -> Result<(), MethodError> {
    let context = request.context;
    let refused = || MethodError::Unsupported("reviewers PHP refuses");
    let self_accept = context
        .config
        .value("differential.allow-self-accept")
        .is_some_and(php::is_truthy);
    if !self_accept
        && revision
            .author_phid
            .as_ref()
            .is_some_and(|author| added.contains(&author.as_str()))
    {
        return Err(refused());
    }
    if !lists::all_valid(request, added).await? {
        return Err(refused());
    }
    Ok(())
}

/// Plans the transaction, which needs `CAN_EDIT`, and updates the
/// reviewers of `revision` and `rows` as the edit leaves them. An edit
/// without effect is left to PHP, which still saves and publishes.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    wanted: &Wanted,
    revision: &mut Revision,
    rows: &mut Vec<ReviewerRow>,
) -> Result<Planned, MethodError> {
    let old: Map = rows
        .iter()
        .map(|row| (row.reviewer.clone(), row.status.clone()))
        .collect();
    let new = new_value(&old, wanted);
    if !has_effect(&old, &new) {
        return Err(MethodError::Unsupported("edits without effect"));
    }
    let context = request.context;
    let can_edit = crate::storage::can_edit(
        &context.database,
        &context.config,
        &request.viewer,
        revision,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if !can_edit {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    let added: Vec<&str> = new
        .iter()
        .filter(|(phid, _)| status_of(&old, phid).is_none())
        .map(|(phid, _)| phid.as_str())
        .collect();
    if !added.is_empty() {
        check_added(request, revision, &added).await?;
    }
    // The rows as reloaded: removed ones go, new ones come last.
    revision
        .reviewers
        .retain(|reviewer| status_of(&new, reviewer.phid.as_str()).is_some());
    rows.retain(|row| status_of(&new, &row.reviewer).is_some());
    for (phid, status) in &new {
        match revision
            .reviewers
            .iter_mut()
            .find(|reviewer| reviewer.phid.as_str() == phid)
        {
            Some(reviewer) => status.clone_into(&mut reviewer.status),
            None => revision
                .reviewers
                .push(Reviewer::new(Phid::new(phid.clone()), status)),
        }
        match rows.iter_mut().find(|row| row.reviewer == *phid) {
            Some(row) => status.clone_into(&mut row.status),
            None => rows.push(ReviewerRow::new(phid.clone(), status.clone())),
        }
    }
    Ok(Planned {
        phid: super::transaction_phid(),
        old,
        new,
    })
}

/// `applyExternalEffects()`: deletes the removed reviewers' rows, then
/// saves the changed ones in the new order.
pub(super) async fn write(
    connection: &mut phab_db::sqlx::MySqlConnection,
    database: &Database,
    revision: &str,
    planned: &Planned,
    now: i64,
) -> Result<(), MethodError> {
    let table = database.table("differential", "differential_reviewer");
    let removed: Vec<&str> = planned
        .old
        .iter()
        .filter(|(phid, _)| status_of(&planned.new, phid).is_none())
        .map(|(phid, _)| phid.as_str())
        .collect();
    if !removed.is_empty() {
        phab_db::bind_all(
            phab_db::sqlx::query(&format!(
                "DELETE FROM {table} WHERE revisionPHID = ? AND reviewerPHID IN ({})",
                phab_db::placeholders(removed.len())
            ))
            .bind(revision),
            removed.iter().copied(),
        )
        .execute(&mut *connection)
        .await?;
    }
    let update = format!(
        "UPDATE {table} SET reviewerStatus = ?, dateModified = ? WHERE revisionPHID = ? AND \
         reviewerPHID = ?"
    );
    let insert = format!(
        "INSERT INTO {table} (revisionPHID, reviewerPHID, reviewerStatus, dateCreated, \
         dateModified, lastActionDiffPHID, lastCommentDiffPHID, lastActorPHID, voidedPHID, \
         options) VALUES (?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, '[]')"
    );
    for (phid, status) in &planned.new {
        let query = match status_of(&planned.old, phid) {
            Some(old) if old == status => continue,
            Some(_) => phab_db::sqlx::query(&update)
                .bind(status)
                .bind(now)
                .bind(revision)
                .bind(phid),
            None => phab_db::sqlx::query(&insert)
                .bind(revision)
                .bind(phid)
                .bind(status)
                .bind(now)
                .bind(now),
        };
        query.execute(&mut *connection).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> Map {
        entries
            .iter()
            .map(|(phid, status)| ((*phid).to_owned(), (*status).to_owned()))
            .collect()
    }

    fn wanted(operation: Operation, tokens: &[(&str, &str)]) -> Wanted {
        Wanted {
            operation,
            tokens: map(tokens),
        }
    }

    #[test]
    fn parses_phids_and_blocking() {
        let wanted = parse(
            "reviewers.add",
            &json!(["PHID-USER-abc", "blocking(PHID-PROJ-def)"]),
        )
        .unwrap();
        assert_eq!(
            wanted.tokens,
            map(&[("PHID-USER-abc", "added"), ("PHID-PROJ-def", "blocking")])
        );
        for value in [
            json!(["alice"]),
            json!(["PHID-DREV-abc"]),
            json!(["blocking( PHID-USER-abc)"]),
            json!(["packages(PHID-USER-abc)"]),
            json!("PHID-USER-abc"),
        ] {
            assert!(parse("reviewers.add", &value).is_err(), "{value}");
        }
    }

    #[test]
    fn new_values_like_php() {
        let old = map(&[("bob", "added"), ("build", "blocking")]);
        assert_eq!(
            new_value(&old, &wanted(Operation::Add, &[("carol", "added")])),
            map(&[("bob", "added"), ("build", "blocking"), ("carol", "added")])
        );
        // Making a reviewer blocking keeps their place.
        assert_eq!(
            new_value(&old, &wanted(Operation::Add, &[("bob", "blocking")])),
            map(&[("bob", "blocking"), ("build", "blocking")])
        );
        assert_eq!(
            new_value(&old, &wanted(Operation::Remove, &[("bob", "blocking")])),
            map(&[("build", "blocking")])
        );
        // Setting starts from nothing, in the request's order.
        assert_eq!(
            new_value(
                &old,
                &wanted(Operation::Set, &[("carol", "added"), ("bob", "added")])
            ),
            map(&[("carol", "added"), ("bob", "added")])
        );
        // Accepts survive being added again.
        let accepted = map(&[("bob", "accepted")]);
        let again = new_value(&accepted, &wanted(Operation::Add, &[("bob", "added")]));
        assert!(!has_effect(&accepted, &again));
    }

    #[test]
    fn encodes_like_php() {
        assert_eq!(encode(&Vec::new()), "[]");
        assert_eq!(
            encode(&map(&[("b", "added"), ("a", "blocking")])),
            r#"{"b":"added","a":"blocking"}"#
        );
    }
}
