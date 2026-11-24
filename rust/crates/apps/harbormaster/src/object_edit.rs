//! `harbormaster.build.edit` (`HarbormasterBuildEditAPIMethod`) and
//! `harbormaster.buildable.edit` (`HarbormasterBuildableEditAPIMethod`),
//! and what they share with `harbormaster.step.edit`
//! ([`crate::step_edit`]): loading the edited object and saving it.
//!
//! `HarbormasterBuildEditEngine` and `HarbormasterBuildableEditEngine`
//! build no field of their own, and the only extension which would apply
//! to builds and buildables, `PhabricatorAuthMFAEditEngineExtension`, is
//! disabled: they have no edit type. Ported: the errors of unknown objects
//! and types, and edits without transactions, which need `CAN_EDIT`, save
//! the object (Lisk writes `dateModified`) and queue the publish task. Left
//! to PHP: creations, and objects the viewer can't see or edit.

use phab_conduit::edit::{self, Identifier, Publication};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::sqlx::MySqlConnection;
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{Capability, ObjectKind, PolicySubject};
use serde_json::Value;

use crate::storage::{self, Build, Buildable, Object, Policies};

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// `newObjectFromIdentifier()` with `CAN_VIEW`: the object, which the
/// viewer must see. An ID or a PHID no row has is PHP's exception;
/// objects the query filters or the policy filter rejects (a policy
/// exception) are left to PHP.
pub(crate) async fn load_object<T: Object>(
    request: &ApiRequest<'_>,
    identifier: &Identifier,
) -> Result<T, MethodError> {
    let (column, value) = match identifier {
        Identifier::Id(id) => ("id", id.to_string()),
        Identifier::Phid(phid) => ("phid", phid.clone()),
    };
    let found: Vec<T> = storage::load_where(&request.context.database, column, &[value]).await?;
    if found.is_empty() {
        return Err(edit::missing_object(request, identifier));
    }
    T::visible(request, found)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| unsupported("objects the viewer can't see"))
}

/// Whether the viewer has `CAN_EDIT` on an object with these (inherited)
/// policies, as `applyCapabilityChecks()` requires of every edit, even
/// one without transactions. Automatic capabilities (like those of a
/// diff's author) are not considered: a refusal is left to PHP, which
/// may grant them, or raises a policy exception.
pub(crate) async fn can_edit(
    request: &ApiRequest<'_>,
    phid: &Phid,
    policies: Option<&Policies>,
) -> Result<bool, MethodError> {
    let Some(policies) = policies else {
        return Ok(false);
    };
    let context = request.context;
    let subject = PolicySubject::new(Some(phid.clone()), ObjectKind::Other)
        .with_policy(Capability::VIEW, policies.view.clone())
        .with_policy(Capability::EDIT, policies.edit.clone());
    let facts = phab_policy::loader::load_facts(
        &context.database,
        &context.config,
        &request.viewer,
        &[&subject],
        &NoExtendedObjects,
        request.now,
    )
    .await?;
    Ok(phab_policy::has_capability(
        &request.viewer,
        &subject,
        Capability::EDIT,
        &facts,
    )?)
}

/// Whether a JSON column as Lisk stores it (`json_encode()` of the
/// decoded dictionary) is what the row holds, so that saving the object
/// writes it back unchanged. Lisk decodes a list as an array too, which
/// only an empty one encodes back as a dictionary.
pub(crate) fn stores_back(stored: &Value, text: &str) -> bool {
    let is_dictionary = match stored {
        Value::Object(_) => true,
        Value::Array(list) => list.is_empty(),
        _ => false,
    };
    is_dictionary && php::json_encode(&php::round_trip(stored)) == text
}

/// Locks the row of `table` with this ID and checks that its JSON
/// `columns` are written back as they are.
pub(crate) async fn lock_row(
    connection: &mut MySqlConnection,
    table: &str,
    id: i64,
    columns: &[&str],
) -> Result<(), MethodError> {
    let select = if columns.is_empty() {
        "id".to_owned()
    } else {
        columns.join(", ")
    };
    let row = phab_db::sqlx::query(&format!(
        "SELECT {select} FROM {table} WHERE id = ? FOR UPDATE"
    ))
    .bind(id)
    .fetch_one(&mut *connection)
    .await?;
    for column in columns {
        let text = phab_db::rows::text(&row, column)?;
        let stored = phab_db::rows::json(&row, column)?;
        if !stores_back(&stored, &text) {
            return Err(unsupported("JSON columns Lisk would encode differently"));
        }
    }
    Ok(())
}

/// An edit of `harbormaster.build.edit` or `harbormaster.buildable.edit`:
/// the edited object's ID, PHID and table, and its JSON columns.
struct Edited {
    id: i64,
    phid: String,
    table: &'static str,
    json_columns: &'static [&'static str],
    /// Whether the viewer can edit it.
    can_edit: bool,
}

/// What both methods do once the object is loaded: check the types, then
/// save the object without transactions.
async fn save_without_transactions(
    request: &ApiRequest<'_>,
    raw: &[edit::RawTransaction],
    edited: Edited,
) -> Result<Value, MethodError> {
    edit::check_types(raw, &[])?;
    if !edited.can_edit {
        return Err(unsupported("objects the viewer can't edit"));
    }
    let actor = crate::writes::actor(request)?;
    let database = &request.context.database;
    let publication = Publication::new(database, actor, edited.phid, Vec::new()).await?;
    let now = request.now;
    let table = database.table("harbormaster", edited.table);
    let mut transaction = database.pool().begin().await?;
    lock_row(&mut transaction, &table, edited.id, edited.json_columns).await?;
    phab_db::sqlx::query(&format!("UPDATE {table} SET dateModified = ? WHERE id = ?"))
        .bind(now)
        .bind(edited.id)
        .execute(&mut *transaction)
        .await?;
    publication
        .schedule(&mut transaction, database, now)
        .await?;
    transaction.commit().await?;
    Ok(publication.response(edited.id))
}

/// `harbormaster.build.edit`.
pub struct BuildEdit;

impl BuildEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let raw = edit::raw_transactions(request)?;
        let Some(identifier) = edit::identifier(request)? else {
            return Err(unsupported("creating builds"));
        };
        let build: Build = load_object(request, &identifier).await?;
        let edited = Edited {
            id: build.id,
            phid: build.phid.as_str().to_owned(),
            table: "harbormaster_build",
            json_columns: &["buildParameters"],
            can_edit: can_edit(request, &build.phid, build.policies.as_ref()).await?,
        };
        save_without_transactions(request, &raw, edited).await
    }
}

impl Method for BuildEdit {
    fn name(&self) -> &'static str {
        "harbormaster.build.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

/// `harbormaster.buildable.edit`.
pub struct BuildableEdit;

impl BuildableEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let raw = edit::raw_transactions(request)?;
        let Some(identifier) = edit::identifier(request)? else {
            return Err(unsupported("creating buildables"));
        };
        let buildable: Buildable = load_object(request, &identifier).await?;
        let edited = Edited {
            id: buildable.id,
            phid: buildable.phid.as_str().to_owned(),
            table: "harbormaster_buildable",
            json_columns: &[],
            can_edit: can_edit(request, &buildable.phid, buildable.policies.as_ref()).await?,
        };
        save_without_transactions(request, &raw, edited).await
    }
}

impl Method for BuildableEdit {
    fn name(&self) -> &'static str {
        "harbormaster.buildable.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn finds_columns_lisk_stores_back() {
        assert!(stores_back(&json!({"a": "b/c"}), "{\"a\":\"b\\/c\"}"));
        assert!(stores_back(&json!([]), "[]"));
        assert!(!stores_back(&json!({"a": "b/c"}), "{\"a\": \"b/c\"}"));
        assert!(!stores_back(&json!(["x"]), "[\"x\"]"));
        assert!(!stores_back(&json!({}), "{}"));
    }
}
