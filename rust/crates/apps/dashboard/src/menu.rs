//! Profile menus (`PhabricatorProfileMenuEngine`) as portals use them:
//! the stored items (`search_profilepanelconfiguration`) merged into the
//! builtin ones and sorted, and the menus a dashboard appears in.

use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value};

/// `PhabricatorDividerProfileMenuItem::MENUITEMKEY`.
pub const DIVIDER: &str = "divider";
/// `PhabricatorLabelProfileMenuItem::MENUITEMKEY`.
pub const LABEL: &str = "label";
/// `PhabricatorLinkProfileMenuItem::MENUITEMKEY`.
pub const LINK: &str = "link";
/// `PhabricatorDashboardProfileMenuItem::MENUITEMKEY`.
pub const DASHBOARD: &str = "dashboard";
/// `PhabricatorManageProfileMenuItem::MENUITEMKEY`.
pub const MANAGE_MENU: &str = "menu.manage";
/// `PhabricatorDashboardPortalMenuItem::MENUITEMKEY`.
pub const MANAGE_PORTAL: &str = "portal";

/// `PhabricatorProfileMenuItemAffectsObjectEdgeType::EDGECONST`.
const ITEM_AFFECTS_OBJECT: i64 = 70;

/// A menu item (`PhabricatorProfileMenuItemConfiguration`), stored or
/// builtin.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// The row ID; `None` for builtin items without a stored version.
    pub id: Option<i64>,
    /// The menu's object.
    pub profile_phid: Phid,
    /// The item type, like `dashboard`.
    pub menu_item_key: String,
    /// The key of the builtin item it is, or stores changes to.
    pub builtin_key: Option<String>,
    /// `menuItemOrder`.
    pub order: Option<i64>,
    /// `visible`, `default` or `disabled`.
    pub visibility: String,
    /// `menuItemProperties`, decoded.
    pub properties: Map<String, Value>,
    /// The user of a personal item.
    pub custom_phid: Option<Phid>,
    /// Builtin tail items go after the others.
    pub is_tail: bool,
}

impl MenuItem {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: Some(rows::int(row, "id")?),
            profile_phid: Phid::new(rows::text(row, "profilePHID")?),
            menu_item_key: rows::text(row, "menuItemKey")?,
            builtin_key: rows::opt_text(row, "builtinKey")?,
            order: rows::opt_int(row, "menuItemOrder")?,
            visibility: rows::text(row, "visibility")?,
            properties: match rows::json(row, "menuItemProperties") {
                Ok(Value::Object(map)) => map,
                _ => Map::new(),
            },
            custom_phid: rows::opt_phid(row, "customPHID")?
                .filter(|phid| !phid.as_str().is_empty()),
            is_tail: false,
        })
    }

    fn builtin(profile: &Phid, key: &str, menu_item_key: &str, order: i64) -> Self {
        Self {
            id: None,
            profile_phid: profile.clone(),
            menu_item_key: menu_item_key.to_owned(),
            builtin_key: Some(key.to_owned()),
            order: Some(order),
            visibility: "visible".to_owned(),
            properties: Map::new(),
            custom_phid: None,
            is_tail: true,
        }
    }

    /// `getMenuItemProperty()` as a string, if it is one.
    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).and_then(Value::as_str)
    }

    /// `getItemIdentifier()`: the ID, or the builtin key.
    pub fn identifier(&self) -> String {
        match self.id {
            Some(id) => id.to_string(),
            None => self.builtin_key.clone().unwrap_or_default(),
        }
    }

    /// `canHideMenuItem()`: the management items can't be hidden.
    fn can_hide(&self) -> bool {
        !matches!(self.menu_item_key.as_str(), MANAGE_MENU | MANAGE_PORTAL)
    }

    /// `isDisabled()`.
    pub fn is_disabled(&self) -> bool {
        self.can_hide() && self.visibility == "disabled"
    }

    /// `isDefault()`.
    pub fn is_default(&self) -> bool {
        self.visibility == "default"
    }

    /// `getSortVector()`.
    fn sort_vector(&self) -> (i64, i64, i64, i64, i64) {
        (
            i64::from(self.custom_phid.is_none()),
            if self.is_tail { 2 } else { 1 },
            i64::from(self.order.is_none()),
            self.order.unwrap_or(0),
            self.id.unwrap_or(0),
        )
    }
}

/// The stored global items of a menu (`withCustomPHIDs(array(null),
/// true)`), in ID order.
async fn load_global_items(database: &Database, profile: &Phid) -> Result<Vec<MenuItem>, DbError> {
    let sql = format!(
        "SELECT id, profilePHID, menuItemKey, builtinKey, menuItemOrder, visibility, \
         menuItemProperties, customPHID FROM {} \
         WHERE profilePHID = ? AND (customPHID IS NULL OR customPHID = '') ORDER BY id DESC",
        database.table("search", "search_profilepanelconfiguration"),
    );
    phab_db::sqlx::query(&sql)
        .bind(profile.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(MenuItem::from_row)
        .collect()
}

/// `PhabricatorDashboardPortalProfileMenuEngine::loadItems(MODE_COMBINED)`:
/// the builtin items (a divider, "Edit Menu", "Manage Portal") with their
/// stored changes, and the stored items, sorted. Stored items of other
/// builtin keys are dropped, as PHP does.
pub async fn portal_items(database: &Database, portal: &Phid) -> Result<Vec<MenuItem>, DbError> {
    let mut items = vec![
        MenuItem::builtin(portal, "tail", DIVIDER, 1),
        MenuItem::builtin(portal, "item.configure", MANAGE_MENU, 2),
        MenuItem::builtin(portal, "manage", MANAGE_PORTAL, 3),
    ];
    for mut stored in load_global_items(database, portal).await? {
        match stored.builtin_key.clone() {
            Some(key) => {
                // Rows come newest first and the last one wins, like PHP.
                if let Some(builtin) = items
                    .iter_mut()
                    .find(|item| item.builtin_key.as_deref() == Some(&key))
                {
                    stored.is_tail = builtin.is_tail;
                    *builtin = stored;
                }
            }
            None => items.push(stored),
        }
    }
    items.sort_by_key(MenuItem::sort_vector);
    Ok(items)
}

/// The items of any menu which point to `object` (`withAffectedObjectPHIDs()`),
/// global or personal to `custom` (`withCustomPHIDs(..., true)`), by ID.
pub async fn items_affecting(
    database: &Database,
    object: &Phid,
    custom: Option<&Phid>,
) -> Result<Vec<MenuItem>, DbError> {
    let custom_condition = if custom.is_some() {
        "(config.customPHID IN (?) OR config.customPHID IS NULL)"
    } else {
        "config.customPHID IS NULL"
    };
    let sql = format!(
        "SELECT config.id, config.profilePHID, config.menuItemKey, config.builtinKey, \
         config.menuItemOrder, config.visibility, config.menuItemProperties, config.customPHID \
         FROM {} config JOIN {} affected ON affected.src = config.phid AND affected.type = ? \
         WHERE {custom_condition} AND affected.dst = ? ORDER BY config.id DESC",
        database.table("search", "search_profilepanelconfiguration"),
        database.table("search", "edge"),
    );
    let mut query = phab_db::sqlx::query(&sql).bind(ITEM_AFFECTS_OBJECT);
    if let Some(custom) = custom {
        query = query.bind(custom.as_str());
    }
    query
        .bind(object.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(MenuItem::from_row)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(id: i64, key: &str, builtin: Option<&str>, order: Option<i64>) -> MenuItem {
        MenuItem {
            id: Some(id),
            builtin_key: builtin.map(str::to_owned),
            order,
            is_tail: false,
            ..MenuItem::builtin(&Phid::new("PHID-PRTL-1"), "x", key, 0)
        }
    }

    #[test]
    fn tail_items_go_last_and_unordered_items_after_ordered_ones() {
        let mut items = [
            MenuItem::builtin(&Phid::new("PHID-PRTL-1"), "tail", DIVIDER, 1),
            stored(5, DASHBOARD, None, None),
            stored(6, LINK, None, Some(9)),
            stored(7, LABEL, None, Some(2)),
        ];
        items.sort_by_key(MenuItem::sort_vector);
        let keys: Vec<String> = items.iter().map(MenuItem::identifier).collect();
        assert_eq!(keys, ["7", "6", "5", "tail"]);
    }

    #[test]
    fn management_items_cannot_be_hidden() {
        let mut item = stored(1, MANAGE_PORTAL, Some("manage"), None);
        item.visibility = "disabled".to_owned();
        assert!(!item.is_disabled());
        item.menu_item_key = LINK.to_owned();
        assert!(item.is_disabled());
    }
}
