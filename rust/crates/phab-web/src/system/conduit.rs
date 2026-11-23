//! `/conduit/`: the method list (`PhabricatorConduitListController` with
//! the builtin queries of `PhabricatorConduitSearchEngine`) and a method's
//! page (`PhabricatorConduitConsoleController`), from the method catalog.
//! The "Call Method" form stays with PHP (the page links to it); methods
//! with documentation pages (the search and edit engines, whose fields
//! depend on the install) and the examples, which show the viewer's API
//! token, stay with PHP.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::has_named_queries;
use phab_policy::ApplicationPolicies;

use super::catalog::{self, Method};
use super::{Body, Item, Link, List, PageBuilder, Property, Section, SystemPage, no_query};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorConduitApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorConduitApplication";

/// `PhabricatorConduitSearchEngine`.
const ENGINE: &str = "PhabricatorConduitSearchEngine";

/// `PhabricatorConduitMethodQuery` as the viewer: methods of installed
/// applications they can see, not internal, with the statuses of the
/// query.
async fn visible_methods<S: WebState>(
    state: &S,
    viewer: &WebUser,
    deprecated: bool,
) -> Result<Vec<&'static Method>, PageError> {
    let policies = ApplicationPolicies::from_config(state.config());
    let methods: Vec<&Method> = catalog::methods()
        .iter()
        .filter(|method| !method.internal)
        .filter(|method| {
            method
                .application
                .as_deref()
                .is_none_or(|class| policies.is_installed(class))
        })
        .filter(|method| match method.status.as_str() {
            "stable" | "frozen" | "unstable" => true,
            "deprecated" => deprecated,
            _ => false,
        })
        .collect();
    let mut classes: Vec<&str> = methods
        .iter()
        .filter_map(|method| method.application.as_deref())
        .collect();
    classes.sort_unstable();
    classes.dedup();
    let visible = viewer.can_view_applications(state, &classes).await?;
    let hidden: Vec<&str> = classes
        .iter()
        .zip(visible)
        .filter(|(_, visible)| !visible)
        .map(|(class, _)| *class)
        .collect();
    Ok(methods
        .into_iter()
        .filter(|method| {
            method
                .application
                .as_deref()
                .is_none_or(|class| !hidden.contains(&class))
        })
        .collect())
}

/// `GET /conduit/`.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if has_named_queries(state.database(), &viewer.user.phid, ENGINE).await? {
        return Err(PageError::Fallback("saved method queries"));
    }
    render_list(state.as_ref(), &viewer, "modern", &uri).await
}

/// `GET /conduit/query/<key>/`.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "modern" => "modern",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved method query")),
    };
    render_list(state.as_ref(), &viewer, key, &uri).await
}

/// The icon and label of a method's status, if it isn't stable.
fn status_tag(status: &str) -> Option<(&'static str, &'static str)> {
    match status {
        "unstable" => Some(("fa-warning remarkup-icon-yellow", "Unstable")),
        "deprecated" => Some(("fa-warning remarkup-icon-red", "Deprecated")),
        "frozen" => Some(("fa-archive", "Frozen")),
        _ => None,
    }
}

async fn render_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    uri: &Uri,
) -> Result<Page<SystemPage>, PageError> {
    no_query(uri)?;
    let methods = visible_methods(state, viewer, key == "all").await?;
    // `renderResultList()`: a list per run of methods of one prefix.
    let mut groups: Vec<(String, Vec<Item>)> = Vec::new();
    for method in methods {
        let item = Item {
            name: method.name.clone(),
            uri: Some(format!("/conduit/method/{}/", method.name)),
            icon: status_tag(&method.status).map(|(icon, _)| icon.to_owned()),
            tags: status_tag(&method.status)
                .map(|(_, label)| vec![label.to_owned()])
                .unwrap_or_default(),
            attributes_html: vec![escape(method.summary.as_deref().unwrap_or(""))],
            disabled: false,
        };
        match groups.last_mut() {
            Some((name, items)) if *name == method.application_name => items.push(item),
            _ => groups.push((method.application_name.clone(), vec![item])),
        }
    }
    let title = if key == "all" {
        "All Methods"
    } else {
        "Modern Methods"
    };
    let mut page = PageBuilder::new("Conduit API")
        .title(title)
        .icon("fa-tty")
        .nav(vec![
            Link::new("Modern Methods", "/conduit/query/modern/").inactive(key == "modern"),
            Link::new("All Methods", "/conduit/query/all/").inactive(key == "all"),
            Link::new("Call Logs", "/conduit/log/"),
        ]);
    if groups.is_empty() {
        page = page.section(Section::untitled(Body::Notice(
            "No methods found.".to_owned(),
        )));
    }
    for (name, items) in groups {
        page = page.section(Section::new(
            name,
            Body::List(List {
                items,
                empty: String::new(),
            }),
        ));
    }
    Ok(page.build(state, viewer, uri))
}

/// `GET /conduit/method/<method>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn method<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(name): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    no_query(&uri)?;
    let state = state.as_ref();
    // `withMethods()` on the unfiltered statuses: deprecated methods have
    // a page too, internal ones as well.
    let policies = ApplicationPolicies::from_config(state.config());
    let method = catalog::methods()
        .iter()
        .find(|method| method.name == name)
        .filter(|method| {
            method
                .application
                .as_deref()
                .is_none_or(|class| policies.is_installed(class))
        })
        .ok_or(PageError::NotFound)?;
    if let Some(class) = method.application.as_deref()
        && !viewer
            .can_view_applications(state, &[class])
            .await?
            .first()
            .copied()
            .unwrap_or(false)
    {
        return Err(PageError::NotFound);
    }
    if method.documentation_pages != 0 {
        return Err(PageError::Fallback("a method with documentation pages"));
    }

    let mut properties = Vec::new();
    let stability = match method.status.as_str() {
        "unstable" => Some((
            "fa-exclamation-triangle remarkup-icon-yellow",
            "Unstable Method",
            "This method is new and unstable. Its interface is subject to change.",
        )),
        "deprecated" => Some((
            "fa-exclamation-triangle remarkup-icon-red",
            "Deprecated Method",
            "This method is deprecated.",
        )),
        "frozen" => Some((
            "fa-archive",
            "Frozen Method",
            "This method is frozen and will eventually be deprecated.",
        )),
        _ => None,
    };
    if let Some((icon, label, default)) = stability {
        let reason = method
            .status_description
            .as_deref()
            .filter(|reason| !reason.is_empty())
            .unwrap_or(default);
        properties.push(Property::html(
            "Stability",
            format!(
                "<span class=\"fa {icon}\" aria-hidden=\"true\"></span> <strong>{label}:</strong> \
                 {}",
                escape(reason)
            ),
        ));
    }
    properties.push(Property::text(
        "Returns",
        method.return_type.as_deref().unwrap_or(""),
    ));
    let mut errors: Vec<String> = method
        .errors
        .iter()
        .map(|error| {
            format!(
                "<li><strong>{}:</strong> {}</li>",
                escape(&error.code),
                escape(error.meaning.as_deref().unwrap_or(""))
            )
        })
        .collect();
    errors.push(
        "<li><strong>ERR-CONDUIT-CORE:</strong> See error message for details.</li>".to_owned(),
    );
    properties.push(Property::html(
        "Errors",
        format!("<ul>{}</ul>", errors.join("")),
    ));
    let (icon, description) = match method.scope.as_str() {
        "scope.always" => (
            "fa-globe remarkup-icon-green",
            "OAuth clients may always call this method.".to_owned(),
        ),
        "scope.never" => (
            "fa-ban remarkup-icon-red",
            "OAuth clients may never call this method.".to_owned(),
        ),
        scope => (
            "fa-unlock-alt remarkup-icon-blue",
            format!(
                "OAuth clients may call this method after requesting access to the \"{scope}\" \
                 scope."
            ),
        ),
    };
    properties.push(Property::html(
        "OAuth Scope",
        format!(
            "<span class=\"fa {icon}\" aria-hidden=\"true\"></span> {}",
            escape(&description)
        ),
    ));

    let description =
        super::remarkup_html(state, &viewer, method.description.as_deref().unwrap_or("")).await?;

    // `PhabricatorConduitConsoleController`'s form is PHP's: it calls the
    // method with the viewer's session.
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    let call = format!(
        "<p>Calling methods from the web uses the old interface: <a href=\"{}\">open the \
         “Call Method” form</a>.</p>",
        escape(&crate::opt_in::off_uri(current))
    );

    Ok(PageBuilder::new(&method.name)
        .icon("fa-tty")
        .section(
            Section::new(
                format!("API Method: {}", method.name),
                Body::Properties(properties),
            )
            .anchor("overview"),
        )
        .section(
            Section::new("Method Description", Body::Html(description)).anchor("documentation"),
        )
        .section(
            Section::new(
                "Parameters",
                Body::Table(super::Table {
                    headers: vec!["Parameter", "Type"],
                    rows: method
                        .params
                        .iter()
                        .map(|param| {
                            vec![
                                escape(&param.name),
                                escape(param.param_type.as_deref().unwrap_or("")),
                            ]
                        })
                        .collect(),
                    empty: "This method takes no parameters.".to_owned(),
                }),
            )
            .anchor("parameters"),
        )
        .section(Section::new("Call Method", Body::Html(call)).anchor("call"))
        .side(Link::new("Overview", "#overview").icon("fa-list"))
        .side(Link::new("Documentation", "#documentation").icon("fa-book"))
        .side(Link::new("Call Method", "#call").icon("fa-play"))
        .build(state, &viewer, &uri))
}
