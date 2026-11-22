//! Dashboards (`PhabricatorDashboardApplication`): portals, dashboards
//! and their panels, the profile menus of portals, and the read-only
//! `portal.search` method. The edit methods (`dashboard.panel.edit`,
//! `portal.edit`) stay in PHP.
//!
//! Portals, dashboards and panels have view and edit policies of their
//! own, without automatic capabilities or extended policies; their queries
//! also need the Dashboards application to be visible.

pub mod menu;
pub mod portal_search;
pub mod storage;

use phab_conduit::{Application, Registry};

/// `PhabricatorDashboardApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorDashboardApplication",
    name: "Dashboards",
};

/// Registers the Dashboards methods.
pub fn register(registry: &mut Registry) {
    registry.register(portal_search::PortalSearch);
}
