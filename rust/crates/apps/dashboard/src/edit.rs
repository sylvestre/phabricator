//! `dashboard.panel.edit` and `portal.edit`: the `name` transactions of
//! `PhabricatorDashboardPanelEditEngine` and
//! `PhabricatorDashboardPortalEditEngine` on existing panels and portals,
//! applied as their editors do (no mail, no feed, no Herald; the publish
//! task indexes the object). The web forms and actions make the same
//! writes with a web content source ([`apply()`]), and also archive or
//! activate panels (`dashpanel:archive`, as
//! `PhabricatorDashboardPanelArchiveController` does) and dashboards
//! (`dashboard:status`, `PhabricatorDashboardArchiveController`).
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

use crate::storage::{self, DASHBOARD, PANEL, PORTAL, Policied};

/// `getColumnMaximumByteLength('name')` of panels (`sort255`) and portals
/// (`text255`).
const NAME_LENGTH: usize = 255;

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// The kind of object an edit changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A panel (`PhabricatorDashboardPanelTransactionEditor`).
    Panel,
    /// A portal (`PhabricatorDashboardPortalEditor`).
    Portal,
    /// A dashboard (`PhabricatorDashboardTransactionEditor`).
    Dashboard,
}

impl Target {
    fn kind(self) -> &'static Kind {
        match self {
            Self::Panel => &PANELS,
            Self::Portal => &PORTALS,
            Self::Dashboard => &DASHBOARDS,
        }
    }
}

/// The transaction of an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Renames a panel or a portal.
    Name(String),
    /// Archives (`true`) or activates a panel.
    PanelArchived(bool),
    /// Sets the status of a dashboard: `active` or `archived`.
    DashboardStatus(&'static str),
}

/// What differs between panels, portals and dashboards.
struct Kind {
    /// The PHID type.
    phid_type: PhidType,
    /// The table of the objects.
    table: &'static str,
    /// The table of their transactions.
    transactions: &'static str,
    /// The column Lisk serializes as JSON, which it encodes again.
    serialized: &'static str,
    /// The column of the archived state: `isArchived` or `status`.
    state: &'static str,
    /// The name transaction, where it is ported.
    name_type: Option<&'static str>,
    /// The object, in the validation errors.
    noun: &'static str,
}

const PANELS: Kind = Kind {
    phid_type: PANEL,
    table: "dashboard_panel",
    transactions: "dashboard_paneltransaction",
    serialized: "properties",
    state: "isArchived",
    name_type: Some("dashpanel:name"),
    noun: "Panel",
};

const PORTALS: Kind = Kind {
    phid_type: PORTAL,
    table: "dashboard_portal",
    transactions: "dashboard_portaltransaction",
    serialized: "properties",
    state: "status",
    name_type: Some("name"),
    noun: "Portal",
};

const DASHBOARDS: Kind = Kind {
    phid_type: DASHBOARD,
    table: "dashboard",
    transactions: "dashboard_transaction",
    serialized: "layoutConfig",
    state: "status",
    name_type: None,
    noun: "Dashboard",
};

/// The edited object.
struct Edited {
    id: i64,
    phid: Phid,
    name: String,
    /// `isArchived` as a boolean, or the `status`.
    state: Value,
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
/// the transactions need (`applyCapabilityChecks()`; the archive
/// controllers query with both).
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
        "SELECT id, phid, name, {} AS state, viewPolicy, editPolicy FROM {} WHERE {column} = ?",
        kind.state,
        database.table("dashboard", kind.table)
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(value)
        .fetch_optional(database.pool())
        .await?
        .ok_or_else(|| unsupported("unknown objects"))?;
    let state = if kind.state == "isArchived" {
        json!(rows::flag(&row, "state")?)
    } else {
        json!(rows::text(&row, "state")?)
    };
    let found = Edited {
        id: rows::int(&row, "id")?,
        phid: Phid::new(rows::text(&row, "phid")?),
        name: rows::text(&row, "name")?,
        state,
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

/// The value of the column an edit sets.
enum ColumnValue {
    Text(String),
    Int(i64),
}

/// The transaction of `change` on `object`, the column it sets and its
/// value.
fn plan(
    kind: &Kind,
    object: &Edited,
    change: Change,
) -> Result<(Planned, &'static str, ColumnValue), MethodError> {
    Ok(match change {
        Change::Name(name) => {
            let name_type = kind
                .name_type
                .ok_or_else(|| unsupported("dashboard names"))?;
            let errors = name_errors(kind, &name);
            if !errors.is_empty() {
                return Err(edit::validation_error(&errors));
            }
            let planned =
                Planned::new(kind.phid_type, name_type, (json!(object.name), json!(name)));
            (planned, "name", ColumnValue::Text(name))
        }
        Change::PanelArchived(archived) if kind.state == "isArchived" => {
            let planned = Planned::new(
                kind.phid_type,
                "dashpanel:archive",
                (object.state.clone(), json!(archived)),
            );
            (planned, "isArchived", ColumnValue::Int(i64::from(archived)))
        }
        Change::DashboardStatus(status) if kind.phid_type == DASHBOARD => {
            let planned = Planned::new(
                kind.phid_type,
                "dashboard:status",
                (object.state.clone(), json!(status)),
            );
            (planned, "status", ColumnValue::Text(status.to_owned()))
        }
        _ => return Err(unsupported("transactions not ported")),
    })
}

/// Applies `change` to the object of `target` which `identifier` names,
/// as its editor does, with the content source `source`
/// (`phab_db::editor::CONDUIT_SOURCE` or `WEB_SOURCE`); the answer is
/// `buildConduitResponse()`'s.
///
/// # Errors
///
/// [`MethodError::Unsupported`], before anything is written, for what
/// is left to PHP (objects the viewer can't see or edit, edits without
/// effect), and PHP's validation errors.
pub async fn apply(
    request: &ApiRequest<'_>,
    target: Target,
    identifier: Identifier,
    change: Change,
    source: &'static str,
) -> Result<Value, MethodError> {
    let kind = target.kind();
    edit::require_writes(request)?;
    let actor = edit::actor(request)?;
    let object = load(request, kind, identifier).await?;
    write(request, (kind, object), change, (actor, source)).await
}

/// Writes `change` on `object`.
async fn write(
    request: &ApiRequest<'_>,
    (kind, object): (&Kind, Edited),
    change: Change,
    (actor, source): (String, &'static str),
) -> Result<Value, MethodError> {
    let (planned, column, value) = plan(kind, &object, change)?;
    if !planned.has_effect() {
        return Err(unsupported("edits without effect"));
    }

    let database = &request.context.database;
    let mut publication = Publication::new(
        database,
        actor,
        object.phid.as_str().to_owned(),
        vec![planned],
    )
    .await?;
    publication.content_source = source;
    let now = request.now;
    let table = database.table("dashboard", kind.table);
    let mut transaction = database.pool().begin().await?;
    // Lisk saves every column: the serialized one is encoded again.
    let serialized = phab_db::sqlx::query(&format!(
        "SELECT {} AS serialized FROM {table} WHERE id = ? FOR UPDATE",
        kind.serialized
    ))
    .bind(object.id)
    .fetch_one(&mut *transaction)
    .await?;
    let serialized = php::json_encode(&php::round_trip(&rows::json(&serialized, "serialized")?));
    let update = format!(
        "UPDATE {table} SET {column} = ?, {} = ?, dateModified = ? WHERE id = ?",
        kind.serialized
    );
    let query = phab_db::sqlx::query(&update);
    let query = match value {
        ColumnValue::Text(text) => query.bind(text),
        ColumnValue::Int(number) => query.bind(number),
    };
    query
        .bind(serialized)
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

/// Renames a panel or a portal.
async fn rename(request: &ApiRequest<'_>, target: Target) -> Result<Value, MethodError> {
    let raw = edit::raw_transactions(request)?;
    let Some(identifier) = edit::identifier(request)? else {
        return Err(unsupported("creating panels and portals"));
    };
    edit::require_writes(request)?;
    let actor = edit::actor(request)?;
    let kind = target.kind();
    let object = load(request, kind, identifier).await?;
    let name = name(&raw)?;
    let source = phab_db::editor::CONDUIT_SOURCE;
    write(request, (kind, object), Change::Name(name), (actor, source)).await
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
        Box::pin(rename(request, Target::Panel))
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
        Box::pin(rename(request, Target::Portal))
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
