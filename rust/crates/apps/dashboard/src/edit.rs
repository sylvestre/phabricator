//! `dashboard.panel.edit` and `portal.edit`: the `name` transactions of
//! `PhabricatorDashboardPanelEditEngine` and
//! `PhabricatorDashboardPortalEditEngine` on existing panels and portals,
//! applied as their editors do (no mail, no feed, no Herald; the publish
//! task indexes the object).
//!
//! Ported edit types: `name`. Left to PHP: creating panels and portals,
//! the other types (policies, projects, the fields of panel types), panels
//! and portals the viewer can't see or edit (PHP raises policy
//! exceptions), and edits without effect.

use phab_conduit::edit::{self, Identifier, Planned, Publication, RawTransaction};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php};
use phab_db::rows;
use serde_json::{Value, json};

use crate::storage::{self, PANEL, PORTAL, Policied};

/// `getColumnMaximumByteLength('name')` of panels (`sort255`) and portals
/// (`text255`).
const NAME_LENGTH: usize = 255;

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// What differs between panels and portals.
struct Kind {
    /// The PHID type.
    phid_type: PhidType,
    /// The table of the objects.
    table: &'static str,
    /// The table of their transactions.
    transactions: &'static str,
    /// The name transaction.
    name_type: &'static str,
    /// The object, in the validation errors.
    noun: &'static str,
}

const PANELS: Kind = Kind {
    phid_type: PANEL,
    table: "dashboard_panel",
    transactions: "dashboard_paneltransaction",
    name_type: "dashpanel:name",
    noun: "Panel",
};

const PORTALS: Kind = Kind {
    phid_type: PORTAL,
    table: "dashboard_portal",
    transactions: "dashboard_portaltransaction",
    name_type: "name",
    noun: "Portal",
};

/// The edited object.
struct Edited {
    id: i64,
    phid: Phid,
    name: String,
    view_policy: String,
    edit_policy: String,
}

impl Policied for Edited {
    fn phid(&self) -> &Phid {
        &self.phid
    }
    fn view_policy(&self) -> &str {
        &self.view_policy
    }
    fn edit_policy(&self) -> &str {
        &self.edit_policy
    }
}

/// The name of the request, its only transaction.
fn name(raw: &[RawTransaction]) -> Result<String, MethodError> {
    match raw {
        [xaction] if xaction.kind == "name" => edit::string("name", &xaction.value),
        [] => Err(unsupported("empty transaction lists")),
        _ => Err(unsupported("transactions not ported")),
    }
}

/// `newObjectFromIdentifier()` with `CAN_VIEW`, then `CAN_EDIT`, which
/// the name transaction needs (`applyCapabilityChecks()`).
async fn load(
    request: &ApiRequest<'_>,
    kind: &Kind,
    identifier: Identifier,
) -> Result<Edited, MethodError> {
    let context = request.context;
    let database = &context.database;
    let (column, value) = match identifier {
        Identifier::Id(id) => ("id", id.to_string()),
        Identifier::Phid(phid) => ("phid", phid),
    };
    let sql = format!(
        "SELECT id, phid, name, viewPolicy, editPolicy FROM {} WHERE {column} = ?",
        database.table("dashboard", kind.table)
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(value)
        .fetch_optional(database.pool())
        .await?
        .ok_or_else(|| unsupported("unknown objects"))?;
    let found = Edited {
        id: rows::int(&row, "id")?,
        phid: Phid::new(rows::text(&row, "phid")?),
        name: rows::text(&row, "name")?,
        view_policy: rows::text(&row, "viewPolicy")?,
        edit_policy: rows::text(&row, "editPolicy")?,
    };
    let visible = storage::visible(
        database,
        &context.config,
        &request.viewer,
        vec![found],
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    let found = visible
        .into_iter()
        .next()
        .ok_or_else(|| unsupported("objects the viewer can't see"))?;
    let can_edit = storage::can_edit(
        database,
        &context.config,
        &request.viewer,
        &found,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if !can_edit {
        return Err(unsupported("objects the viewer can't edit"));
    }
    Ok(found)
}

/// The errors of the name transaction's `validateTransactions()`.
fn name_errors(kind: &Kind, name: &str) -> Vec<String> {
    if name.is_empty() {
        vec![format!("{}s must have a title.", kind.noun)]
    } else if name.len() > NAME_LENGTH {
        vec![format!(
            "{} names must not be longer than {NAME_LENGTH} characters.",
            kind.noun
        )]
    } else {
        Vec::new()
    }
}

/// Renames a panel or a portal.
async fn rename(request: &ApiRequest<'_>, kind: &Kind) -> Result<Value, MethodError> {
    let raw = edit::raw_transactions(request)?;
    let Some(identifier) = edit::identifier(request)? else {
        return Err(unsupported("creating panels and portals"));
    };
    edit::require_writes(request)?;
    let actor = edit::actor(request)?;
    let object = load(request, kind, identifier).await?;
    let name = name(&raw)?;
    let errors = name_errors(kind, &name);
    if !errors.is_empty() {
        return Err(edit::validation_error(&errors));
    }
    let planned = Planned::new(
        kind.phid_type,
        kind.name_type,
        (json!(object.name), json!(name)),
    );
    if !planned.has_effect() {
        return Err(unsupported("edits without effect"));
    }

    let database = &request.context.database;
    let publication = Publication::new(
        database,
        actor,
        object.phid.as_str().to_owned(),
        vec![planned],
    )
    .await?;
    let now = request.now;
    let table = database.table("dashboard", kind.table);
    let mut transaction = database.pool().begin().await?;
    // Lisk saves every column: the properties are encoded again.
    let properties = phab_db::sqlx::query(&format!(
        "SELECT properties FROM {table} WHERE id = ? FOR UPDATE"
    ))
    .bind(object.id)
    .fetch_one(&mut *transaction)
    .await?;
    let properties = php::json_encode(&php::round_trip(&rows::json(&properties, "properties")?));
    phab_db::sqlx::query(&format!(
        "UPDATE {table} SET name = ?, properties = ?, dateModified = ? WHERE id = ?"
    ))
    .bind(&name)
    .bind(properties)
    .bind(now)
    .bind(object.id)
    .execute(&mut *transaction)
    .await?;
    let transactions = database.table("dashboard", kind.transactions);
    publication
        .write_transactions(&mut transaction, &transactions, now)
        .await?;
    publication
        .schedule(&mut transaction, database, now)
        .await?;
    transaction.commit().await?;
    Ok(publication.response(object.id))
}

/// `dashboard.panel.edit`.
pub struct PanelEdit;

impl Method for PanelEdit {
    fn name(&self) -> &'static str {
        "dashboard.panel.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(rename(request, &PANELS))
    }
}

/// `portal.edit`.
pub struct PortalEdit;

impl Method for PortalEdit {
    fn name(&self) -> &'static str {
        "portal.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(rename(request, &PORTALS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names_like_php() {
        assert_eq!(name_errors(&PANELS, ""), ["Panels must have a title."]);
        assert_eq!(
            name_errors(&PORTALS, &"x".repeat(256)),
            ["Portal names must not be longer than 255 characters."]
        );
        assert_eq!(name_errors(&PORTALS, &"x".repeat(255)).len(), 0);
    }
}
