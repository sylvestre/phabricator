//! `/favorites/` (`PhabricatorFavoritesMenuItemController`), read-only:
//! the viewer's favorites menu (`PhabricatorFavoritesProfileMenuEngine`),
//! its builtin "Create Project" and "Create Repository" forms and the
//! items stored for everyone or the viewer. Configuring the menu
//! (`/favorites/menu/...`) stays with PHP, and so do items of types not
//! ported and installs with configured project or repository forms.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::Uri;
use phab_app_dashboard::menu::{self, MenuItem};
use phab_app_dashboard::storage as dashboard;
use phab_core::{Phid, hash};
use phab_policy::{Capability, ObjectKind, POLICY_ADMIN, PolicySubject, Viewer, loader};

use crate::error::PageError;
use crate::harbormaster;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorFavoritesApplication`.
const APPLICATION: &str = "PhabricatorFavoritesApplication";
/// `PhabricatorEditEngineProfileMenuItem::MENUITEMKEY`.
const EDIT_FORM: &str = "editengine";
/// `PhabricatorProjectProfileMenuItem::MENUITEMKEY`.
const PROJECT: &str = "project";

/// A builtin quick-create form: its engine, application, create
/// capability and its default, form name and URI.
struct QuickCreate {
    engine: &'static str,
    application: &'static str,
    capability: (&'static str, &'static str),
    name: &'static str,
    icon: &'static str,
    uri: &'static str,
}

/// The engines whose default form is a quick-create item
/// (`isDefaultQuickCreateEngine()`), by `getQuickCreateOrderVector()`.
const QUICK_CREATE: &[QuickCreate] = &[
    QuickCreate {
        engine: "projects.project",
        application: "PhabricatorProjectApplication",
        capability: ("project.create", phab_policy::POLICY_USER),
        name: "Create Project",
        icon: "fa-briefcase",
        uri: "/project/edit/form/default/",
    },
    QuickCreate {
        engine: "diffusion.repository",
        application: "PhabricatorDiffusionApplication",
        capability: ("diffusion.create", POLICY_ADMIN),
        name: "Create Repository",
        icon: "fa-code",
        uri: "/diffusion/edit/form/default/",
    },
];

/// An entry of the menu.
pub(crate) enum Entry {
    Link {
        name: String,
        icon: String,
        uri: String,
    },
    Label(String),
    Divider,
}

#[derive(Template)]
#[template(path = "favorites.html")]
pub(crate) struct FavoritesPage {
    shell: Shell,
    entries: Vec<Entry>,
}

/// Whether `viewer` has the application capability `capability` of
/// `class`, whose default is `default`.
async fn has_application_capability<S: WebState>(
    state: &S,
    viewer: &Viewer,
    class: &str,
    (capability, default): (&'static str, &'static str),
) -> Result<bool, PageError> {
    let capability = Capability(capability);
    let applications = phab_policy::ApplicationPolicies::from_config(state.config());
    let policy = applications.custom_policy(class, capability, default);
    let mut subject = PolicySubject::new(
        Some(Phid::new(format!("PHID-APPS-{class}"))),
        ObjectKind::Other,
    )
    .with_policy(capability, policy);
    subject.capabilities.push(capability);
    let facts = loader::load_facts(
        state.database(),
        state.config(),
        viewer,
        &[&subject],
        &loader::NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    phab_policy::has_capability(viewer, &subject, capability, &facts)
        .map_err(harbormaster::internal)
}

/// Whether forms of these engines are configured: the builtin forms
/// may then be renamed, disabled or replaced.
async fn has_configured_forms<S: WebState>(state: &S) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT id FROM {} WHERE engineKey IN (?, ?) LIMIT 1",
        database.table("search", "search_editengineconfiguration")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(QUICK_CREATE[0].engine)
        .bind(QUICK_CREATE[1].engine)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// The name a stored item gives itself, or `default`.
fn named(item: &MenuItem, default: &str) -> String {
    item.property("name")
        .filter(|name| !name.is_empty())
        .unwrap_or(default)
        .to_owned()
}

/// The entries of the menu, as the viewer sees them.
async fn entries<S: WebState>(
    state: &S,
    viewer: &WebUser,
    items: &[MenuItem],
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    let mut entries = Vec::new();
    for item in items.iter().filter(|item| !item.is_disabled()) {
        let entry = match item.menu_item_key.as_str() {
            menu::DIVIDER => Some(Entry::Divider),
            menu::LABEL => Some(Entry::Label(named(item, ""))),
            menu::LINK => {
                let uri = item.property("uri").unwrap_or_default();
                // `PhabricatorLinkProfileMenuItem::isValidLinkURI()`.
                let valid = ["http://", "https://", "/"]
                    .iter()
                    .any(|prefix| uri.starts_with(prefix))
                    && !uri.starts_with("//");
                valid.then(|| Entry::Link {
                    name: named(item, ""),
                    icon: "fa-link".to_owned(),
                    uri: uri.to_owned(),
                })
            }
            menu::MANAGE_MENU => Some(Entry::Link {
                name: named(item, "Edit Favorites"),
                icon: "fa-pencil".to_owned(),
                uri: "/favorites/menu/configure/".to_owned(),
            }),
            menu::DASHBOARD => {
                let phid = Phid::new(item.property("dashboardPHID").unwrap_or_default());
                let found =
                    dashboard::load_dashboards(database, std::slice::from_ref(&phid)).await?;
                let visible =
                    dashboard::visible(database, state.config(), &as_viewer, found, state.now())
                        .await
                        .map_err(harbormaster::internal)?;
                visible.first().map(|found| Entry::Link {
                    name: named(item, &found.name),
                    icon: found.icon.clone(),
                    uri: found.uri(),
                })
            }
            PROJECT => {
                let phid = Phid::new(item.property("project").unwrap_or_default());
                let found =
                    phab_app_project::storage::load_by_phid(database, std::slice::from_ref(&phid))
                        .await?;
                let (visible, graph) = phab_app_project::policy::visible_projects(
                    database,
                    state.config(),
                    &as_viewer,
                    found,
                    state.now(),
                )
                .await
                .map_err(harbormaster::internal)?;
                visible.first().map(|found| Entry::Link {
                    name: named(item, &found.display_name(graph.parent(found))),
                    icon: "fa-briefcase".to_owned(),
                    uri: found.uri(),
                })
            }
            EDIT_FORM => {
                let form_key = item.property("formKey").unwrap_or_default();
                let Some(form) = QUICK_CREATE
                    .iter()
                    .find(|form| form_key == format!("{}/default", form.engine))
                else {
                    return Err(PageError::Fallback("favorites with other forms"));
                };
                let usable = harbormaster::can_use(state, viewer, form.application).await?
                    && has_application_capability(
                        state,
                        &as_viewer,
                        form.application,
                        form.capability,
                    )
                    .await?;
                usable.then(|| Entry::Link {
                    name: named(item, form.name),
                    icon: form.icon.to_owned(),
                    uri: form.uri.to_owned(),
                })
            }
            _ => return Err(PageError::Fallback("favorites of types not ported")),
        };
        entries.extend(entry);
    }
    Ok(entries)
}

/// `GET /favorites/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<FavoritesPage>, PageError> {
    let state = state.as_ref();
    viewer.require_application(state, APPLICATION).await?;
    if has_configured_forms(state).await? {
        return Err(PageError::Fallback("configured quick-create forms"));
    }
    let profile = Phid::new(format!("PHID-APPS-{APPLICATION}"));
    let mut builtins = Vec::new();
    for form in QUICK_CREATE {
        let form_key = format!("{}/default", form.engine);
        let builtin_key = format!(
            "editengine.form({})",
            hash::digest_for_index(form_key.as_bytes())
        );
        builtins.push((builtin_key, EDIT_FORM, Some(form_key)));
    }
    let items =
        menu::favorites_items(state.database(), &profile, &viewer.user.phid, builtins).await?;
    let entries = entries(state, &viewer, &items).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(FavoritesPage {
        shell: Shell::for_user(state.config(), &viewer, "Favorites", current),
        entries,
    }))
}
