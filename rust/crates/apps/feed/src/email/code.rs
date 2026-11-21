//! `ResolveCodeChange`: what an update changed in the code.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::events::TransactionList;
use super::store::{RawChangeset, RevisionRow, Store};
use super::{Fail, Result, production_uri};

/// `DifferentialChangeType::FILE_TEXT`.
const FILE_TEXT: i64 = 1;
/// `DifferentialChangeType::TYPE_ADD`.
const TYPE_ADD: i64 = 1;
/// `DifferentialChangeType::TYPE_CHANGE`.
const TYPE_CHANGE: i64 = 2;
/// `DifferentialChangeType::TYPE_DELETE`.
const TYPE_DELETE: i64 = 3;

/// A diff PHID from a transaction value; `find(string $PHID)` throws on
/// anything else.
fn diff_phid(value: &Value) -> Result<phab_core::Phid> {
    match value {
        Value::String(phid) => Ok(phab_core::Phid::new(phid)),
        Value::Null => Err(Fail::Throw("an update without diff")),
        _ => Err(Fail::Unsupported("an unusual diff in an update")),
    }
}

/// `PhabricatorDiffStore::find()`, whose return type throws for a missing
/// diff.
async fn find_diff(store: &mut Store<'_>, value: &Value) -> Result<super::store::DiffRow> {
    let phid = diff_phid(value)?;
    store
        .diff(&phid)
        .await?
        .ok_or(Fail::Throw("a diff which doesn't exist"))
}

/// A decoded JSON value as PHP compares arrays with `!==` after `ksort()`.
fn sorted(value: &Value) -> Result<BTreeMap<String, Value>> {
    match value {
        Value::Object(map) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Value::Array(items) if items.is_empty() => Ok(BTreeMap::new()),
        Value::Null => Err(Fail::Throw("ksort() of null properties")),
        _ => Err(Fail::Unsupported("unusual changeset properties")),
    }
}

/// `DifferentialChangeset::hasSameEffectAs()`.
fn has_same_effect(new: &RawChangeset, old: &RawChangeset) -> Result<bool> {
    if new.filename != old.filename {
        return Ok(false);
    }
    let hash = |changeset: &RawChangeset| match changeset.metadata.get("hash.effect") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(hash)) => Ok(Some(hash.clone())),
        Some(_) => Err(Fail::Unsupported("an unusual effect hash")),
    };
    let (Some(u), Some(v)) = (hash(new)?, hash(old)?) else {
        return Ok(false);
    };
    if u != v {
        return Ok(false);
    }
    Ok(sorted(&new.new_properties)? == sorted(&old.new_properties)?)
}

/// `resolveAnyDiffChanges()`.
pub(crate) async fn any_diff_changes(
    store: &mut Store<'_>,
    transactions: &TransactionList,
) -> Result<bool> {
    if transactions
        .attempt_with_type("differential.revision.summary")?
        .is_some()
    {
        return Ok(true);
    }
    let update = transactions.with_type("differential:update")?;
    let old = find_diff(store, &update.old_value).await?;
    let new = find_diff(store, &update.new_value).await?;
    if old.changesets.len() != new.changesets.len() {
        return Ok(true);
    }
    for (old, new) in old.changesets.iter().zip(&new.changesets) {
        if old.file_type != new.file_type || old.change_type != new.change_type {
            return Ok(true);
        }
        if (new.file_type != FILE_TEXT || new.change_type != TYPE_CHANGE)
            && !has_same_effect(new, old)?
        {
            return Ok(true);
        }
        if old.hunk_data.len() != new.hunk_data.len() {
            return Ok(true);
        }
        for (old, new) in old.hunk_data.iter().zip(&new.hunk_data) {
            let old_lines: Vec<&[u8]> = old.split(|byte| *byte == b'\n').collect();
            let new_lines: Vec<&[u8]> = new.split(|byte| *byte == b'\n').collect();
            if old_lines.len() != new_lines.len() {
                return Ok(true);
            }
            for (old, new) in old_lines.iter().zip(&new_lines) {
                let changed = |line: &[u8]| line.first().is_some_and(|first| *first != b' ');
                if old != new && (changed(old) || changed(new)) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// `EmailAffectedFile::from()` of each changeset of the active diff.
pub(crate) async fn affected_files(
    store: &mut Store<'_>,
    revision: &RevisionRow,
) -> Result<Vec<Value>> {
    let phid = revision
        .active_diff_phid
        .as_ref()
        .ok_or(Fail::Throw("a revision without active diff"))?;
    let diff = store
        .diff(phid)
        .await?
        .ok_or(Fail::Throw("an active diff which doesn't exist"))?;
    Ok(diff
        .changesets
        .iter()
        .map(|changeset| {
            let change = match changeset.change_type {
                TYPE_ADD => "added",
                TYPE_DELETE => "removed",
                _ => "modified",
            };
            json!({"path": format!("/{}", changeset.display_filename()), "change": change})
        })
        .collect())
}

/// `resolveNewChangesLink()`.
pub(crate) async fn new_changes_link(
    store: &mut Store<'_>,
    transactions: &TransactionList,
    revision: &RevisionRow,
) -> Result<String> {
    let update = transactions.with_type("differential:update")?;
    let original = find_diff(store, &update.old_value).await?;
    let current = revision
        .active_diff_id
        .ok_or(Fail::Throw("a revision whose active diff is gone"))?;
    production_uri(
        store.config,
        &format!("/{}?vs={}&id={current}", revision.monogram(), original.id),
    )
}
