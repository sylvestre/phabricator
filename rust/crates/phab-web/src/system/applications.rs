//! `/applications/` (`PhabricatorApplicationsListController` with the
//! builtin queries of `PhabricatorAppSearchEngine`) and
//! `/applications/view/<class>/` (`PhabricatorApplicationDetailViewController`).
//! Editing policies and installing applications are PHP's forms.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::has_named_queries;
use phab_policy::{ApplicationPolicies, Capability};

use super::catalog::{self, Application, GROUPS};
use super::policy::{self, Described};
use super::{Body, Item, Link, List, PageBuilder, Property, Section, SystemPage, no_query};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorApplicationsApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorApplicationsApplication";

/// `PhabricatorAppSearchEngine`.
const ENGINE: &str = "PhabricatorAppSearchEngine";

/// The builtin queries.
fn navigation(current: &str) -> Vec<Link> {
    vec![
        Link::new("Launcher", "/applications/query/launcher/").inactive(current == "launcher"),
        Link::new("All Applications", "/applications/query/all/").inactive(current == "all"),
    ]
}

/// `GET /applications/`.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if has_named_queries(state.database(), &viewer.user.phid, ENGINE).await? {
        return Err(PageError::Fallback("saved application queries"));
    }
    render_list(state.as_ref(), &viewer, "launcher", &uri).await
}

/// `GET /applications/query/<key>/`.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "launcher" => "launcher",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved application query")),
    };
    render_list(state.as_ref(), &viewer, key, &uri).await
}

/// The applications of a query, as `PhabricatorApplicationQuery` filters
/// them: listed, the launcher's installed and launchable, and visible to
/// the viewer.
async fn render_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    uri: &Uri,
) -> Result<Page<SystemPage>, PageError> {
    no_query(uri)?;
    let policies = ApplicationPolicies::from_config(state.config());
    let mut applications: Vec<&Application> = catalog::applications()
        .iter()
        .filter(|application| !application.unlisted)
        .filter(|application| {
            key != "launcher"
                || (policies.is_installed(&application.class) && application.launchable)
        })
        .collect();
    let classes: Vec<&str> = applications.iter().map(|app| app.class.as_str()).collect();
    let visible = viewer.can_view_applications(state, &classes).await?;
    let mut visible = visible.into_iter();
    applications.retain(|_| visible.next().unwrap_or(false));
    // `msort($all_applications, 'getName')`, a stable sort.
    applications.sort_by(|left, right| left.name.cmp(&right.name));

    let groups: Vec<(&str, Vec<&Application>)> = if key == "launcher" {
        let mut groups: Vec<(&str, Vec<&Application>)> = GROUPS
            .iter()
            .map(|(group, name)| {
                (
                    *name,
                    applications
                        .iter()
                        .copied()
                        .filter(|application| application.group == *group)
                        .collect::<Vec<_>>(),
                )
            })
            .filter(|(_, members)| !members.is_empty())
            .collect();
        let others: Vec<&Application> = applications
            .iter()
            .copied()
            .filter(|application| !GROUPS.iter().any(|(group, _)| application.group == *group))
            .collect();
        if !others.is_empty() {
            return Err(PageError::Fallback("an application of an unknown group"));
        }
        groups.retain(|(_, members)| !members.is_empty());
        groups
    } else {
        vec![("", applications)]
    };

    let title = if key == "launcher" {
        "Launcher"
    } else {
        "All Applications"
    };
    let show_headers = groups.len() > 1;
    let mut page = PageBuilder::new("Applications")
        .title(title)
        .icon("fa-globe")
        .nav(navigation(key));
    for (name, members) in groups {
        let items = members
            .iter()
            .map(|application| application_item(application, &policies))
            .collect();
        let body = Body::List(List {
            items,
            empty: "No applications found.".to_owned(),
        });
        page = page.section(if show_headers {
            Section::new(name, body)
        } else {
            Section::untitled(body)
        });
    }
    Ok(page.build(state, viewer, uri))
}

/// An application of the list.
fn application_item(application: &Application, policies: &ApplicationPolicies) -> Item {
    let installed = policies.is_installed(&application.class);
    let mut tags = Vec::new();
    if !application.first_party {
        tags.push("Extension".to_owned());
    }
    if application.prototype {
        tags.push("Prototype".to_owned());
    }
    let mut attributes = vec![escape(
        application.short_description.as_deref().unwrap_or(""),
    )];
    if !installed {
        attributes.push("Uninstalled".to_owned());
    }
    attributes.push(format!(
        "<a class=\"hm-button\" href=\"/applications/view/{}/\"><span class=\"fa fa-gears\" \
         aria-hidden=\"true\"></span> Configure</a>",
        escape(&application.class)
    ));
    Item {
        name: application.name.clone(),
        uri: application.base_uri.clone().filter(|_| installed),
        icon: Some(
            application
                .icon
                .clone()
                .unwrap_or_else(|| "fa-globe".to_owned()),
        ),
        tags,
        attributes_html: attributes,
        disabled: !installed,
    }
}

/// The capabilities of an application: view, edit and its own.
pub(crate) fn capabilities(application: &Application) -> Vec<(String, String)> {
    let mut list = vec![
        ("view".to_owned(), "Can Use Application".to_owned()),
        ("edit".to_owned(), "Can Configure Application".to_owned()),
    ];
    for capability in &application.capabilities {
        list.push((
            capability.key.clone(),
            capability.label.clone().unwrap_or_default(),
        ));
    }
    list
}

/// `getPolicy($capability)` of an application.
pub(crate) fn application_policy(
    application: &Application,
    policies: &ApplicationPolicies,
    capability: &str,
    allow_public: bool,
) -> String {
    match capability {
        "view" => policies
            .policy(&application.class, Capability::VIEW, allow_public)
            .to_owned(),
        "edit" => policies
            .policy(&application.class, Capability::EDIT, allow_public)
            .to_owned(),
        other => {
            let custom = application
                .capabilities
                .iter()
                .find(|capability| capability.key == other);
            let default = custom.map_or("users", |capability| capability.default.as_str());
            if custom.is_some_and(|capability| !capability.editable) {
                return default.to_owned();
            }
            policies
                .custom_policy(&application.class, Capability(leak(other)), default)
                .to_owned()
        }
    }
}

/// Capabilities are `&'static str` in the policy code; the catalog's live
/// as long as the process.
fn leak(capability: &str) -> &'static str {
    catalog::applications()
        .iter()
        .flat_map(|application| &application.capabilities)
        .find(|custom| custom.key == capability)
        .map_or("", |custom| custom.key.as_str())
}

/// Whether the application has transactions, which the page's timeline
/// would show (policy edits, rendered with PHP's policy names).
async fn has_transactions<S: WebState>(state: &S, phid: &str) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE objectPHID = ? LIMIT 1",
        database.table("application", "application_applicationtransaction")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// `GET /applications/view/<class>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn view<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(class): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    no_query(&uri)?;
    let state = state.as_ref();
    let application = catalog::application(&class).ok_or(PageError::NotFound)?;
    if !viewer
        .can_view_applications(state, &[application.class.as_str()])
        .await?
        .first()
        .copied()
        .unwrap_or(false)
    {
        return Err(PageError::NotFound);
    }
    // `PhabricatorApplicationConfigurationPanel`s: the storage engines.
    if matches!(
        application.class.as_str(),
        "PhabricatorFilesApplication" | "PhabricatorSearchApplication"
    ) {
        return Err(PageError::Fallback("an application configuration panel"));
    }
    if has_transactions(state, &application.phid()).await? {
        return Err(PageError::Fallback("an application with policy edits"));
    }
    let config = state.config();
    let policies = ApplicationPolicies::from_config(config);
    let installed = policies.is_installed(&application.class);

    let mut details = vec![Property::text(
        "Description",
        application.short_description.as_deref().unwrap_or(""),
    )];
    if let Some(flavor) = application
        .flavor_text
        .as_deref()
        .filter(|flavor| !flavor.is_empty())
    {
        details.push(Property::html("", format!("<em>{}</em>", escape(flavor))));
    }
    if application.prototype {
        details.push(Property::html(
            "Prototype",
            "This application is a prototype. <a href=\"https://secure.phabricator.com/\
             diviner/find/?name=User%20Guide%3A%20Prototype%20Applications&amp;type=article&amp;\
             jump=1\" target=\"_blank\">Learn More</a>"
                .to_owned(),
        ));
    }
    let mut page = PageBuilder::new(&application.name)
        .icon(
            application
                .icon
                .clone()
                .unwrap_or_else(|| "fa-globe".to_owned()),
        )
        .tag(if installed {
            "Installed"
        } else {
            "Uninstalled"
        })
        .section(Section::new("Details", Body::Properties(details)));
    if let Some(overview) = application
        .overview
        .as_deref()
        .filter(|overview| !overview.is_empty())
    {
        page = page.section(Section::new(
            "Overview",
            Body::Html(super::remarkup_html(state, &viewer, overview).await?),
        ));
    }

    let capabilities = capabilities(application);
    let identifiers: Vec<String> = capabilities
        .iter()
        .map(|(capability, _)| {
            application_policy(application, &policies, capability, config.allow_public())
        })
        .collect();
    let described = policy::describe(state, &viewer, &identifiers).await?;
    let properties = capabilities
        .iter()
        .zip(&identifiers)
        .map(|((capability, label), identifier)| {
            let description: &Described = &described[identifier];
            Property::html(
                label.clone(),
                description.capability_link(&application.phid(), capability),
            )
        })
        .collect();
    page = page.section(Section::new("Policies", Body::Properties(properties)));

    let can_edit = policy::can_edit_application(state, &viewer, application).await?;
    let class = escape(&application.class);
    page = page.side(
        Link::new("Edit Policies", format!("/applications/edit/{class}/"))
            .icon("fa-pencil")
            .inactive(!can_edit),
    );
    if application.can_uninstall {
        let prototypes = config.truthy("phabricator.show-prototypes");
        page = if installed {
            page.side(
                Link::new("Uninstall", format!("/applications/{class}/uninstall/"))
                    .icon("fa-times")
                    .inactive(!can_edit),
            )
        } else {
            page.side(
                Link::new("Install", format!("/applications/{class}/install/"))
                    .icon("fa-plus")
                    .inactive(!can_edit || (application.prototype && !prototypes)),
            )
        };
    } else {
        page = page.side(
            Link::new("Uninstall", format!("/applications/{class}/uninstall/"))
                .icon("fa-times")
                .inactive(true),
        );
    }
    Ok(page.build(state, &viewer, &uri))
}
