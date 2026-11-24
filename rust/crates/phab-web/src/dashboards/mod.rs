//! Dashboards and portals (`PhabricatorDashboardApplication`), read-only:
//! `/dashboard/view/<id>/`, `/portal/view/<id>/`, the panel page `/W<id>`
//! and the builtin queries of the dashboard and panel lists. Edits, menu
//! configuration and the panel workflows link to PHP.
//!
//! Panels render as `PhabricatorDashboardPanelRenderingEngine` does: a
//! panel the viewer can't see is a "Restricted Panel" box, text panels
//! show their remarkup and tab panels their tabs. Query panels, which PHP
//! renders asynchronously with its search engines, and panels nested
//! deeper than PHP allows send the page to PHP.

mod configure;
mod dashboard;
mod edit;
mod lists;
mod portal;
mod portals;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use phab_app_dashboard::storage::{self, Dashboard, Panel};
use phab_core::Phid;
use phab_policy::Viewer;
use phab_remarkup::Mode;
use serde_json::Value;

pub(crate) use configure::{
    favorites as configure_favorites, home as configure_home, portal as configure_portal,
};
pub(crate) use dashboard::show as dashboard;
pub(crate) use edit::routes as edit_routes;
pub(crate) use lists::{
    dashboard_query, dashboards as dashboard_list, panel, panel_query, panels as panel_list,
    parse_panel_monogram,
};
pub(crate) use portal::{show as portal, show_item as portal_item};
pub(crate) use portals::{console, portal_query, portals as portal_list};

use crate::diff_html::escape;
use crate::error::PageError;
use crate::remarkup;
use crate::state::WebState;

/// `PhabricatorDashboardApplication`.
pub(crate) const APPLICATION: &str = phab_app_dashboard::APPLICATION.class;

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

/// A page ID from the route: digits, or the page doesn't exist.
fn parse_id(id: &str) -> Result<i64, PageError> {
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
        .ok_or(PageError::NotFound)
}

/// How panel headers show (`HEADER_MODE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderMode {
    /// The panel name.
    Normal,
    /// No header: the panels of tabs.
    None,
    /// The monogram and name, with the panel commands: dashboards the
    /// viewer can edit.
    Edit,
}

/// The panels of a page, as the viewer may see them.
struct Panels {
    /// The panels the viewer can see, by PHID.
    visible: HashMap<Phid, Panel>,
    /// The PHIDs of the panels which exist.
    existing: HashSet<Phid>,
}

impl Panels {
    async fn load<S: WebState>(
        state: &S,
        viewer: &Viewer,
        phids: &[Phid],
    ) -> Result<Self, PageError> {
        let database = state.database();
        let panels = storage::load_panels(database, phids).await?;
        let existing = panels.iter().map(|panel| panel.phid.clone()).collect();
        let visible = storage::visible(database, state.config(), viewer, panels, state.now())
            .await
            .map_err(internal)?
            .into_iter()
            .map(|panel| (panel.phid.clone(), panel))
            .collect();
        Ok(Self { visible, existing })
    }
}

/// What a panel is placed in, for its commands in edit mode.
struct Placement<'a> {
    /// The dashboard.
    context: &'a Phid,
    /// The panel's key on it.
    panel_key: &'a str,
}

/// `PhabricatorDashboardPanelRenderingEngine`, without its asynchronous
/// rendering: what the page needs to show the panels.
struct PanelRenderer<'a, S> {
    state: &'a S,
    viewer: &'a Viewer,
}

impl<S: WebState> PanelRenderer<'_, S> {
    /// `renderPanel()`: the panel's box, as HTML.
    async fn render(
        &self,
        panel: Option<&Panel>,
        exists: bool,
        mode: HeaderMode,
        placement: Option<&Placement<'_>>,
        parents: &[Phid],
    ) -> Result<String, PageError> {
        let Some(panel) = panel else {
            let (title, body) = if exists {
                (
                    "Restricted Panel",
                    "You do not have permission to see this panel.",
                )
            } else {
                (
                    "Invalid Panel",
                    "This panel is invalid or does not exist. It may have been deleted.",
                )
            };
            return Ok(error_box(title, body, mode, None, placement));
        };
        if !matches!(panel.panel_type.as_str(), "text" | "query" | "tabs") {
            return Ok(error_box(
                &panel.name,
                &format!(
                    "This panel has type \"{}\", but that panel type is unknown.",
                    panel.panel_type
                ),
                mode,
                Some(panel),
                placement,
            ));
        }
        // `detectRenderingCycle()` throws, and PHP shows the exception.
        if parents.len() >= 4 || parents.contains(&panel.phid) {
            return Err(PageError::Fallback("nested dashboard panels"));
        }
        let content = match panel.panel_type.as_str() {
            "text" => self.text(panel).await?,
            "tabs" => Box::pin(self.tabs(panel, parents)).await?,
            // `shouldRenderAsync()`: PHP fills them in with a request of
            // their own, through its search engines.
            _ => return Err(PageError::Fallback("query panels")),
        };
        Ok(panel_box(
            header_html(panel, mode, placement).as_deref(),
            &content,
        ))
    }

    /// `PhabricatorDashboardTextPanelType::renderPanelContent()`.
    async fn text(&self, panel: &Panel) -> Result<String, PageError> {
        let text = match panel.property("text") {
            None | Some(Value::Null) => "",
            Some(Value::String(text)) => text.as_str(),
            Some(_) => return Err(PageError::Fallback("an unusual text panel")),
        };
        // `{W12}` embeds a panel, which the Rust remarkup doesn't do.
        if text.contains("{W") || text.contains("{w") {
            return Err(PageError::Fallback("a panel embedded in a text panel"));
        }
        let markup = remarkup::render(self.state, self.viewer, text, Mode::Html).await?;
        Ok(format!(
            "<div class=\"remarkup\">{}</div>",
            String::from_utf8_lossy(markup.as_bytes())
        ))
    }

    /// `PhabricatorDashboardTabsPanelType::renderPanelContent()`, with
    /// every tab shown under its name, as the page has no script to
    /// switch between them.
    async fn tabs(&self, panel: &Panel, parents: &[Phid]) -> Result<String, PageError> {
        let tabs = tab_configuration(panel)?;
        if tabs.is_empty() {
            return Ok(
                "<p class=\"muted dashboard-empty\">This tab panel does not have any tabs \
                 yet.</p>"
                    .to_owned(),
            );
        }
        let ids: Vec<i64> = tabs.iter().filter_map(|tab| tab.panel_id).collect();
        let database = self.state.database();
        let subpanels = storage::load_panels_by_ids(database, &ids).await?;
        let subpanels: HashMap<i64, Panel> = storage::visible(
            database,
            self.state.config(),
            self.viewer,
            subpanels,
            self.state.now(),
        )
        .await
        .map_err(internal)?
        .into_iter()
        .map(|panel| (panel.id, panel))
        .collect();
        let mut parents = parents.to_vec();
        parents.push(panel.phid.clone());

        let mut names = String::new();
        let mut contents = String::new();
        for tab in &tabs {
            let subpanel = tab.panel_id.and_then(|id| subpanels.get(&id));
            let name = match (&tab.name, subpanel) {
                (Some(name), _) if !name.is_empty() => name.clone(),
                (_, Some(subpanel)) if !subpanel.name.is_empty() => subpanel.name.clone(),
                _ => "Unnamed Tab".to_owned(),
            };
            let content = match subpanel {
                Some(subpanel) => {
                    Box::pin(self.render(Some(subpanel), true, HeaderMode::None, None, &parents))
                        .await?
                }
                None => "(Invalid Panel)".to_owned(),
            };
            let _ = write!(names, "<li>{}</li>", escape(&name));
            let _ = write!(
                contents,
                "<section class=\"dashboard-tab\"><h3>{}</h3>{content}</section>",
                escape(&name)
            );
        }
        Ok(format!(
            "<ul class=\"dashboard-tabs\">{names}</ul>{contents}"
        ))
    }
}

/// A tab of a tab panel.
struct Tab {
    panel_id: Option<i64>,
    name: Option<String>,
}

/// `getPanelConfiguration()`: the tabs, from a list or a JSON string.
fn tab_configuration(panel: &Panel) -> Result<Vec<Tab>, PageError> {
    let decoded;
    let config = match panel.property("config") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(text)) => {
            decoded = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
            &decoded
        }
        Some(other) => other,
    };
    let specs: Vec<&Value> = match config {
        Value::Array(list) => list.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => return Ok(Vec::new()),
    };
    let scalar = |value: Option<&Value>| -> Option<String> {
        match value? {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    };
    specs
        .into_iter()
        .map(|spec| {
            // PHP's idx() fails on anything but an array.
            let spec = spec
                .as_object()
                .ok_or(PageError::Fallback("an unusual tab panel"))?;
            Ok(Tab {
                panel_id: scalar(spec.get("panelID")).and_then(|id| id.parse().ok()),
                name: scalar(spec.get("name")),
            })
        })
        .collect()
}

/// The header of a panel's box.
fn header_html(
    panel: &Panel,
    mode: HeaderMode,
    placement: Option<&Placement<'_>>,
) -> Option<String> {
    match mode {
        HeaderMode::None => None,
        HeaderMode::Normal => Some(escape(&panel.name)),
        HeaderMode::Edit => Some(format!(
            "{} {}",
            escape(&format!("{} {}", panel.monogram(), panel.name)),
            panel_commands(Some(panel), placement)
        )),
    }
}

/// The "Manage Panel" commands of edit mode (`addPanelHeaderActions()`),
/// links to PHP's workflows.
fn panel_commands(panel: Option<&Panel>, placement: Option<&Placement<'_>>) -> String {
    let mut links = Vec::new();
    if let Some(panel) = panel {
        let mut edit = format!("/dashboard/panel/edit/{}/", panel.id);
        if let Some(placement) = placement {
            let _ = write!(
                edit,
                "?contextPHID={}",
                query_value(placement.context.as_str())
            );
        }
        links.push(command_link(&edit, "fa-pencil", "Edit Panel"));
        links.push(command_link(
            &format!("/{}", panel.monogram()),
            "fa-window-maximize",
            "View Panel Details",
        ));
    }
    if let Some(placement) = placement {
        links.push(command_link(
            &format!(
                "/dashboard/adjust/remove/?contextPHID={}&panelKey={}",
                query_value(placement.context.as_str()),
                query_value(placement.panel_key)
            ),
            "fa-times",
            "Remove Panel",
        ));
    }
    format!(
        "<span class=\"dashboard-commands\">{}</span>",
        links.join(" ")
    )
}

fn query_value(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn command_link(uri: &str, icon: &str, name: &str) -> String {
    format!(
        "<a class=\"hm-button\" href=\"{}\"><span class=\"fa {icon}\" aria-hidden=\"true\"></span> \
         {name}</a>",
        escape(uri)
    )
}

/// `renderPanelDiv()`.
fn panel_box(header_html: Option<&str>, content_html: &str) -> String {
    let header = header_html.map_or_else(String::new, |header| format!("<h2>{header}</h2>"));
    format!("<section class=\"dashboard-box\">{header}{content_html}</section>")
}

/// `renderErrorPanel()`.
fn error_box(
    title: &str,
    body: &str,
    mode: HeaderMode,
    panel: Option<&Panel>,
    placement: Option<&Placement<'_>>,
) -> String {
    let header = match mode {
        HeaderMode::None => None,
        HeaderMode::Normal => Some(escape(title)),
        HeaderMode::Edit => Some(format!(
            "{} {}",
            escape(title),
            panel_commands(panel, placement)
        )),
    };
    panel_box(
        header.as_deref(),
        &format!(
            "<p class=\"dashboard-error\"><span class=\"fa fa-warning remarkup-icon-red\" \
             aria-hidden=\"true\"></span> {}</p>",
            escape(body)
        ),
    )
}

/// A column of a rendered dashboard.
pub(crate) struct ColumnView {
    /// `half`, `third`, `thirds` or `full`.
    pub(crate) class: &'static str,
    /// The panels, as HTML.
    pub(crate) panels: Vec<String>,
    /// In edit mode, where the column's "Create Panel" and "Add Existing
    /// Panel" go.
    pub(crate) add: Option<(String, String)>,
}

/// `PhabricatorDashboardRenderingEngine::renderDashboard()`: the columns
/// of `dashboard` with their panels. `edit` is the arrange mode of the
/// dashboard page, for viewers who can edit it.
async fn render_dashboard<S: WebState>(
    state: &S,
    viewer: &Viewer,
    dashboard: &Dashboard,
    edit: bool,
) -> Result<Vec<ColumnView>, PageError> {
    let panels = Panels::load(state, viewer, &dashboard.panel_phids()).await?;
    let panel_renderer = PanelRenderer { state, viewer };
    let mode = if edit {
        HeaderMode::Edit
    } else {
        HeaderMode::Normal
    };
    let mut columns = Vec::new();
    for column in dashboard.columns() {
        let mut rendered = Vec::new();
        for panel_ref in &column.panels {
            let placement = Placement {
                context: &dashboard.phid,
                panel_key: &panel_ref.panel_key,
            };
            rendered.push(
                panel_renderer
                    .render(
                        panels.visible.get(&panel_ref.panel_phid),
                        panels.existing.contains(&panel_ref.panel_phid),
                        mode,
                        Some(&placement),
                        &[],
                    )
                    .await?,
            );
        }
        let add = edit.then(|| {
            let context = query_value(dashboard.phid.as_str());
            (
                format!(
                    "/dashboard/panel/edit/?contextPHID={context}&columnKey={}",
                    column.key
                ),
                format!(
                    "/dashboard/adjust/add/?contextPHID={context}&columnKey={}",
                    column.key
                ),
            )
        });
        columns.push(ColumnView {
            class: column.class.unwrap_or("full"),
            panels: rendered,
            add,
        });
    }
    Ok(columns)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn panel(config: &Value) -> Panel {
        Panel {
            id: 1,
            phid: Phid::new("PHID-DSHP-1"),
            name: "Tabs".to_owned(),
            panel_type: "tabs".to_owned(),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            is_archived: false,
            properties: json!({"config": config}).as_object().unwrap().clone(),
        }
    }

    #[test]
    fn tabs_come_from_a_list_or_a_json_string() {
        let tabs = tab_configuration(&panel(&json!([{"panelID": "2", "name": "A"}]))).unwrap();
        assert_eq!(tabs[0].panel_id, Some(2));
        assert_eq!(tabs[0].name.as_deref(), Some("A"));
        let tabs = tab_configuration(&panel(&json!("{\"x\":{\"panelID\":3}}"))).unwrap();
        assert_eq!(tabs[0].panel_id, Some(3));
        assert!(
            tab_configuration(&panel(&json!("nope")))
                .unwrap()
                .is_empty()
        );
        assert!(tab_configuration(&panel(&json!([1]))).is_err());
    }
}
