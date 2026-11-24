//! The rows of portals, dashboards and panels, who sees them, and the
//! panel layout of dashboards (`PhabricatorDashboardPanelRefList`).

use std::collections::HashMap;

use phab_core::{Config, Phid, PhidType};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{AutomaticCapability, Capability, ObjectKind, PolicySubject, Viewer};
use serde_json::{Map, Value};

/// `PhabricatorDashboardPortalPHIDType`.
pub const PORTAL: PhidType = PhidType::new(*b"PRTL");
/// `PhabricatorDashboardDashboardPHIDType`.
pub const DASHBOARD: PhidType = PhidType::new(*b"DSHB");
/// `PhabricatorDashboardPanelPHIDType`.
pub const PANEL: PhidType = PhidType::new(*b"DSHP");

/// Why visibility could not be decided.
#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    /// Loading the policy facts failed.
    #[error(transparent)]
    Loader(#[from] phab_policy::loader::LoaderError),
    /// The policy filter refused the check.
    #[error(transparent)]
    Policy(#[from] phab_policy::PolicyError),
}

/// A row with view and edit policies of its own.
pub trait Policied {
    /// The PHID.
    fn phid(&self) -> &Phid;
    /// `getPolicy(CAN_VIEW)`.
    fn view_policy(&self) -> &str;
    /// `getPolicy(CAN_EDIT)`.
    fn edit_policy(&self) -> &str;

    /// The row as a policy object.
    fn subject(&self) -> PolicySubject {
        PolicySubject::new(Some(self.phid().clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy())
            .with_policy(Capability::EDIT, self.edit_policy())
    }
}

/// A `dashboard_portal` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Portal {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// `active` or `archived`.
    pub status: String,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// `dateCreated`.
    pub date_created: i64,
    /// `dateModified`.
    pub date_modified: i64,
}

impl Portal {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            status: rows::text(row, "status")?,
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
        })
    }

    /// `getURI()`.
    pub fn uri(&self) -> String {
        format!("/portal/view/{}/", self.id)
    }
}

impl Policied for Portal {
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

/// A `dashboard` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Dashboard {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// `active` or `archived`.
    pub status: String,
    /// The icon, like `fa-dashboard`.
    pub icon: String,
    /// Who created it.
    pub author_phid: Phid,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// `layoutConfig`, decoded.
    pub layout_config: Value,
}

impl Dashboard {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            status: rows::text(row, "status")?,
            icon: rows::text(row, "icon")?,
            author_phid: Phid::new(rows::text(row, "authorPHID")?),
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            layout_config: rows::json(row, "layoutConfig").unwrap_or(Value::Null),
        })
    }

    /// `isArchived()`.
    pub fn is_archived(&self) -> bool {
        self.status == "archived"
    }

    /// `getURI()`.
    pub fn uri(&self) -> String {
        format!("/dashboard/view/{}/", self.id)
    }

    /// `getPanelRefList()->getColumns()`: the columns of the layout mode,
    /// with the panels placed in them.
    pub fn columns(&self) -> Vec<Column> {
        panel_columns(&self.layout_config)
    }

    /// `getPanelPHIDs()`: the panels, each once.
    pub fn panel_phids(&self) -> Vec<Phid> {
        let mut phids: Vec<Phid> = Vec::new();
        for column in self.columns() {
            for panel in column.panels {
                if !phids.contains(&panel.panel_phid) {
                    phids.push(panel.panel_phid);
                }
            }
        }
        phids
    }
}

impl Policied for Dashboard {
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

/// A column of a dashboard's layout (`PhabricatorDashboardColumn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// `main`, `left` or `right`.
    pub key: &'static str,
    /// The width class: `half`, `third` or `thirds`; none for a single
    /// column.
    pub class: Option<&'static str>,
    /// The panels, in order.
    pub panels: Vec<PanelRef>,
}

/// A panel placed on a dashboard (`PhabricatorDashboardPanelRef`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelRef {
    /// The panel.
    pub panel_phid: Phid,
    /// The key of this placement.
    pub panel_key: String,
}

/// The columns of a layout mode (`getLayoutModeColumns()`); unknown modes
/// get the first mode by order, the full width one.
fn layout_columns(mode: Option<&str>) -> Vec<Column> {
    let column = |key, class| Column {
        key,
        class,
        panels: Vec::new(),
    };
    match mode {
        Some("layout-mode-half-and-half") => {
            vec![column("left", Some("half")), column("right", Some("half"))]
        }
        Some("layout-mode-third-and-thirds") => {
            vec![
                column("left", Some("third")),
                column("right", Some("thirds")),
            ]
        }
        Some("layout-mode-thirds-and-third") => {
            vec![
                column("left", Some("thirds")),
                column("right", Some("third")),
            ]
        }
        _ => vec![column("main", None)],
    }
}

/// A value as `strlen()` and string keys see it.
fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(true) => Some("1".to_owned()),
        _ => None,
    }
}

/// `PhabricatorDashboardPanelRefList::newFromDictionary()`.
fn panel_columns(config: &Value) -> Vec<Column> {
    let config = config.as_object();
    let mode = config
        .and_then(|config| config.get("layoutMode"))
        .and_then(Value::as_str);
    let mut columns = layout_columns(mode);
    let panels: Vec<&Value> = match config.and_then(|config| config.get("panels")) {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(Value::Object(map)) => map.values().collect(),
        _ => Vec::new(),
    };
    let mut seen: Vec<String> = Vec::new();
    for panel in panels {
        let Some(panel) = panel.as_object() else {
            continue;
        };
        let Some(phid) = scalar_string(panel.get("panelPHID")).filter(|phid| !phid.is_empty())
        else {
            continue;
        };
        let Some(key) = scalar_string(panel.get("panelKey")).filter(|key| !key.is_empty()) else {
            continue;
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let column_key = scalar_string(panel.get("columnKey"));
        let index = columns
            .iter()
            .position(|column| Some(column.key) == column_key.as_deref())
            .unwrap_or(0);
        columns[index].panels.push(PanelRef {
            panel_phid: Phid::new(phid),
            panel_key: key,
        });
    }
    columns
}

/// A `dashboard_panel` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// `text`, `query`, `tabs`...
    pub panel_type: String,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// `isArchived`.
    pub is_archived: bool,
    /// `properties`, decoded.
    pub properties: Map<String, Value>,
}

impl Panel {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            panel_type: rows::text(row, "panelType")?,
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            is_archived: rows::flag(row, "isArchived")?,
            properties: match rows::json(row, "properties") {
                Ok(Value::Object(map)) => map,
                _ => Map::new(),
            },
        })
    }

    /// `getMonogram()`.
    pub fn monogram(&self) -> String {
        format!("W{}", self.id)
    }

    /// `getProperty()`.
    pub fn property(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }
}

impl Policied for Panel {
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

const PORTAL_COLUMNS: &str = "id, phid, name, status, viewPolicy, editPolicy, dateCreated, \
                              dateModified";
const DASHBOARD_COLUMNS: &str = "id, phid, name, status, icon, authorPHID, viewPolicy, \
                                 editPolicy, layoutConfig";
const PANEL_COLUMNS: &str =
    "id, phid, name, panelType, viewPolicy, editPolicy, isArchived, properties";

async fn load_where<T>(
    database: &Database,
    table: &str,
    columns: &str,
    column: &str,
    values: Vec<Value>,
    from_row: fn(&MySqlRow) -> Result<T, DbError>,
) -> Result<Vec<T>, DbError> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {columns} FROM {} WHERE {column} IN ({}) ORDER BY id DESC",
        database.table("dashboard", table),
        phab_db::placeholders(values.len()),
    );
    let mut query = phab_db::sqlx::query(&sql);
    for value in &values {
        query = match value {
            Value::Number(number) => query.bind(number.as_i64().unwrap_or_default()),
            other => query.bind(other.as_str().unwrap_or_default().to_owned()),
        };
    }
    query
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(from_row)
        .collect()
}

fn ids(ids: &[i64]) -> Vec<Value> {
    ids.iter().map(|id| Value::from(*id)).collect()
}

fn phids(phids: &[Phid]) -> Vec<Value> {
    phids
        .iter()
        .map(|phid| Value::from(phid.as_str()))
        .collect()
}

/// The portals with these IDs, newest first.
pub async fn load_portals_by_ids(
    database: &Database,
    portal_ids: &[i64],
) -> Result<Vec<Portal>, DbError> {
    load_where(
        database,
        "dashboard_portal",
        PORTAL_COLUMNS,
        "id",
        ids(portal_ids),
        Portal::from_row,
    )
    .await
}

/// The portals with these PHIDs, newest first.
pub async fn load_portals(database: &Database, portals: &[Phid]) -> Result<Vec<Portal>, DbError> {
    load_where(
        database,
        "dashboard_portal",
        PORTAL_COLUMNS,
        "phid",
        phids(portals),
        Portal::from_row,
    )
    .await
}

/// The dashboards with these IDs, newest first.
pub async fn load_dashboards_by_ids(
    database: &Database,
    dashboard_ids: &[i64],
) -> Result<Vec<Dashboard>, DbError> {
    load_where(
        database,
        "dashboard",
        DASHBOARD_COLUMNS,
        "id",
        ids(dashboard_ids),
        Dashboard::from_row,
    )
    .await
}

/// The dashboards with these PHIDs, newest first.
pub async fn load_dashboards(
    database: &Database,
    dashboards: &[Phid],
) -> Result<Vec<Dashboard>, DbError> {
    load_where(
        database,
        "dashboard",
        DASHBOARD_COLUMNS,
        "phid",
        phids(dashboards),
        Dashboard::from_row,
    )
    .await
}

/// The panels with these PHIDs, newest first.
pub async fn load_panels(database: &Database, panels: &[Phid]) -> Result<Vec<Panel>, DbError> {
    load_where(
        database,
        "dashboard_panel",
        PANEL_COLUMNS,
        "phid",
        phids(panels),
        Panel::from_row,
    )
    .await
}

/// The panels with these IDs, newest first.
pub async fn load_panels_by_ids(
    database: &Database,
    panel_ids: &[i64],
) -> Result<Vec<Panel>, DbError> {
    load_where(
        database,
        "dashboard_panel",
        PANEL_COLUMNS,
        "id",
        ids(panel_ids),
        Panel::from_row,
    )
    .await
}

/// `PhabricatorDashboardPanelUsedByObjectEdgeType`.
const PANEL_USED_BY_OBJECT: i64 = 72;

/// The objects using a panel, dashboards and tab panels, newest first
/// (`loadDestinationPHIDs()`).
pub async fn panel_users(database: &Database, panel: &Phid) -> Result<Vec<Phid>, DbError> {
    let sql = format!(
        "SELECT dst FROM {} WHERE src = ? AND type = ? ORDER BY dateCreated DESC, seq DESC",
        database.table("dashboard", "edge"),
    );
    phab_db::sqlx::query(&sql)
        .bind(panel.as_str())
        .bind(PANEL_USED_BY_OBJECT)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| rows::text(row, "dst").map(Phid::new))
        .collect()
}

/// A builtin list query of dashboards or panels (`authored`, `open` or
/// `active`, `all`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListFilter {
    /// `authorPHIDs`.
    pub author: Option<Phid>,
    /// Only active dashboards (`statuses`) or panels (`withArchived(false)`).
    pub active_only: bool,
}

/// A page of rows of `table` matching `filter`, newest first, before the
/// row `before` (the list's cursor).
async fn load_page<T>(
    database: &Database,
    (table, columns, active): (&str, &str, &str),
    filter: &ListFilter,
    (before, limit): (Option<i64>, usize),
    from_row: fn(&MySqlRow) -> Result<T, DbError>,
) -> Result<Vec<T>, DbError> {
    let mut conditions = vec!["1 = 1".to_owned()];
    if filter.author.is_some() {
        conditions.push("authorPHID = ?".to_owned());
    }
    if filter.active_only {
        conditions.push(active.to_owned());
    }
    if before.is_some() {
        conditions.push("id < ?".to_owned());
    }
    let sql = format!(
        "SELECT {columns} FROM {} WHERE {} ORDER BY id DESC LIMIT {limit}",
        database.table("dashboard", table),
        conditions.join(" AND "),
    );
    let mut query = phab_db::sqlx::query(&sql);
    if let Some(author) = &filter.author {
        query = query.bind(author.as_str());
    }
    if let Some(before) = before {
        query = query.bind(before);
    }
    query
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(from_row)
        .collect()
}

/// A page of dashboards (`PhabricatorDashboardQuery`), newest first.
pub async fn dashboard_page(
    database: &Database,
    filter: &ListFilter,
    page: (Option<i64>, usize),
) -> Result<Vec<Dashboard>, DbError> {
    let spec = ("dashboard", DASHBOARD_COLUMNS, "status = 'active'");
    load_page(database, spec, filter, page, Dashboard::from_row).await
}

/// A page of portals (`PhabricatorDashboardPortalQuery`, which the only
/// builtin query, `all`, doesn't filter), newest first.
pub async fn portal_page(
    database: &Database,
    page: (Option<i64>, usize),
) -> Result<Vec<Portal>, DbError> {
    let spec = ("dashboard_portal", PORTAL_COLUMNS, "1 = 1");
    load_page(
        database,
        spec,
        &ListFilter::default(),
        page,
        Portal::from_row,
    )
    .await
}

/// A page of panels (`PhabricatorDashboardPanelQuery`), newest first.
pub async fn panel_page(
    database: &Database,
    filter: &ListFilter,
    page: (Option<i64>, usize),
) -> Result<Vec<Panel>, DbError> {
    let spec = ("dashboard_panel", PANEL_COLUMNS, "isArchived = 0");
    load_page(database, spec, filter, page, Panel::from_row).await
}

/// Which of `subjects` `viewer` has every one of `capabilities` on.
pub async fn filter_subjects(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    subjects: &[PolicySubject],
    capabilities: &[Capability],
    now: i64,
) -> Result<Vec<bool>, VisibilityError> {
    if subjects.is_empty() {
        return Ok(Vec::new());
    }
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts =
        phab_policy::loader::load_facts(database, config, viewer, &refs, &NoExtendedObjects, now)
            .await?;
    Ok(phab_policy::filter(viewer, &refs, capabilities, &facts)?)
}

/// The rows of `rows` `viewer` can see, in their order.
pub async fn visible<T: Policied>(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    rows: Vec<T>,
    now: i64,
) -> Result<Vec<T>, VisibilityError> {
    let subjects: Vec<PolicySubject> = rows.iter().map(Policied::subject).collect();
    let passed = filter_subjects(
        database,
        config,
        viewer,
        &subjects,
        &[Capability::VIEW],
        now,
    )
    .await?;
    Ok(rows
        .into_iter()
        .zip(passed)
        .filter_map(|(row, passed)| passed.then_some(row))
        .collect())
}

/// Whether `viewer` can edit `row`.
pub async fn can_edit<T: Policied>(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    row: &T,
    now: i64,
) -> Result<bool, VisibilityError> {
    let subject = row.subject();
    Ok(filter_subjects(
        database,
        config,
        viewer,
        std::slice::from_ref(&subject),
        &[Capability::EDIT],
        now,
    )
    .await?
    .first()
    .copied()
    .unwrap_or(false))
}

/// `PhabricatorProjectObjectHasProjectEdgeType::EDGECONST`.
const OBJECT_HAS_PROJECT: i64 = 41;

/// The projects of `objects` (portals, dashboards or panels), newest edge
/// first like `PhabricatorEdgeQuery`.
pub async fn projects(
    database: &Database,
    objects: &[Phid],
) -> Result<HashMap<Phid, Vec<Phid>>, DbError> {
    let mut found: HashMap<Phid, Vec<Phid>> = HashMap::new();
    if objects.is_empty() {
        return Ok(found);
    }
    let sql = format!(
        "SELECT src, dst FROM {} WHERE src IN ({}) AND type = ? \
         ORDER BY dateCreated DESC, seq DESC",
        database.table("dashboard", "edge"),
        phab_db::placeholders(objects.len()),
    );
    let query = phab_db::bind_all(phab_db::sqlx::query(&sql), objects.iter().map(Phid::as_str))
        .bind(OBJECT_HAS_PROJECT);
    for row in query.fetch_all(database.pool()).await? {
        found
            .entry(Phid::new(rows::text(&row, "src")?))
            .or_default()
            .push(Phid::new(rows::text(&row, "dst")?));
    }
    Ok(found)
}

/// A `dashboard_transaction` or `dashboard_portaltransaction` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Transaction {
    /// The PHID.
    pub phid: Phid,
    /// Who made it.
    pub author_phid: Phid,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The comment, if any.
    pub comment_phid: Option<Phid>,
    /// `transactionType`.
    pub transaction_type: String,
    /// The decoded old value.
    pub old_value: Value,
    /// The decoded new value.
    pub new_value: Value,
    /// The decoded metadata.
    pub metadata: Map<String, Value>,
    /// `dateCreated`.
    pub date_created: i64,
}

impl Transaction {
    /// `PhabricatorApplicationTransaction` as a policy object: its author
    /// can always see it.
    pub fn subject(&self) -> PolicySubject {
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone());
        subject.automatic.push(AutomaticCapability {
            principal: self.author_phid.clone(),
            capabilities: None,
        });
        subject
    }

    /// `getMetadataValue()` as a boolean, like `core.create`.
    pub fn metadata_flag(&self, key: &str) -> bool {
        self.metadata
            .get(key)
            .is_some_and(phab_core::php::is_truthy)
    }
}

/// The transactions of a dashboard, oldest first.
pub async fn dashboard_transactions(
    database: &Database,
    object: &Phid,
) -> Result<Vec<Transaction>, DbError> {
    object_transactions(database, ("dashboard", "dashboard_transaction"), object).await
}

/// The transactions of `object` in the transaction table `(database,
/// table)`, like `("dashboard", "dashboard_paneltransaction")` or `("user",
/// "user_transaction")`, oldest first.
pub async fn object_transactions(
    database: &Database,
    (schema, table): (&str, &str),
    object: &Phid,
) -> Result<Vec<Transaction>, DbError> {
    let sql = format!(
        "SELECT phid, authorPHID, viewPolicy, editPolicy, commentPHID, transactionType, \
         oldValue, newValue, metadata, dateCreated FROM {} WHERE objectPHID = ? ORDER BY id ASC",
        database.table(schema, table),
    );
    phab_db::sqlx::query(&sql)
        .bind(object.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok(Transaction {
                phid: Phid::new(rows::text(row, "phid")?),
                author_phid: Phid::new(rows::text(row, "authorPHID")?),
                view_policy: rows::text(row, "viewPolicy")?,
                edit_policy: rows::text(row, "editPolicy")?,
                comment_phid: rows::opt_phid(row, "commentPHID")?,
                transaction_type: rows::text(row, "transactionType")?,
                old_value: rows::json(row, "oldValue").unwrap_or(Value::Null),
                new_value: rows::json(row, "newValue").unwrap_or(Value::Null),
                metadata: match rows::json(row, "metadata") {
                    Ok(Value::Object(map)) => map,
                    _ => Map::new(),
                },
                date_created: rows::int(row, "dateCreated")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn panels_go_to_their_column_or_the_first() {
        let columns = panel_columns(&json!({
            "layoutMode": "layout-mode-half-and-half",
            "panels": [
                {"panelKey": "a", "panelPHID": "PHID-DSHP-1", "columnKey": "right"},
                {"panelKey": "b", "panelPHID": "PHID-DSHP-2", "columnKey": "nope"},
                {"panelKey": "a", "panelPHID": "PHID-DSHP-3", "columnKey": "left"},
                {"panelKey": "", "panelPHID": "PHID-DSHP-4"},
                {"panelKey": "c", "panelPHID": "PHID-DSHP-1"},
            ],
        }));
        assert_eq!(columns.len(), 2);
        let keys = |column: &Column| -> Vec<String> {
            column
                .panels
                .iter()
                .map(|panel| panel.panel_key.clone())
                .collect()
        };
        assert_eq!(keys(&columns[0]), ["b", "c"]);
        assert_eq!(keys(&columns[1]), ["a"]);
        assert_eq!(columns[1].class, Some("half"));
    }

    #[test]
    fn unknown_layouts_are_one_column() {
        let columns = panel_columns(&json!([]));
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].key, "main");
        let columns = panel_columns(&json!({"layoutMode": "bogus"}));
        assert_eq!(columns[0].key, "main");
    }
}
