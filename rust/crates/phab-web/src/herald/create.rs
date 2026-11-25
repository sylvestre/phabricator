//! `/herald/new/` and `/herald/create/` (`HeraldNewController`): choosing
//! what a new rule is about, then its type; a global or personal rule goes
//! on to the rule editor (`/herald/edit/?content_type=...&rule_type=...`).
//! Object rules, which ask for their object, are left to PHP.

use std::fmt::Write as _;
use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};

use super::APPLICATION;
use crate::error::PageError;
use crate::harbormaster;
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// A Herald adapter, as the menus describe it.
pub(crate) struct Adapter {
    /// `getAdapterContentType()`.
    pub(crate) content_type: &'static str,
    /// `getAdapterContentName()`.
    pub(crate) name: &'static str,
    /// `getAdapterContentDescription()`, whose lines the menu breaks.
    description: &'static str,
    /// `getAdapterApplicationClass()`.
    application: &'static str,
    /// `supportsRuleType()`: global, object, personal.
    rule_types: &'static [&'static str],
}

/// Every adapter (`HeraldAdapter::getAllAdapters()`).
pub(crate) const ADAPTERS: &[Adapter] = &[
    Adapter {
        content_type: "commit",
        name: "Commits",
        description: "React to new commits appearing in tracked repositories.\nCommit rules can \
                      send email, flag commits, trigger audits, and run build plans.",
        application: "PhabricatorDiffusionApplication",
        rule_types: &["global", "object", "personal"],
    },
    Adapter {
        content_type: "HeraldPreCommitRefAdapter",
        name: "Commit Hook: Branches/Tags/Bookmarks",
        description: "React to branches and tags being pushed to hosted repositories.\nHook \
                      rules can block changes and send push summary mail.",
        application: "PhabricatorDiffusionApplication",
        rule_types: &["global", "object", "personal"],
    },
    Adapter {
        content_type: "HeraldPreCommitContentAdapter",
        name: "Commit Hook: Commit Content",
        description: "React to commits being pushed to hosted repositories.\nHook rules can \
                      block changes and send push summary mail.",
        application: "PhabricatorDiffusionApplication",
        rule_types: &["global", "object", "personal"],
    },
    Adapter {
        content_type: "PhabricatorProjectHeraldAdapter",
        name: "Projects",
        description: "React to projects being created or updated.",
        application: "PhabricatorProjectApplication",
        rule_types: &["global", "personal"],
    },
    Adapter {
        content_type: "PhabricatorMailOutboundMailHeraldAdapter",
        name: "Outbound Mail",
        description: "Route outbound email.",
        application: "PhabricatorMetaMTAApplication",
        rule_types: &["global", "personal"],
    },
    Adapter {
        content_type: "PhabricatorCalendarEventHeraldAdapter",
        name: "Calendar Events",
        description: "React to events being created or updated.",
        application: "PhabricatorCalendarApplication",
        rule_types: &["global", "personal"],
    },
    Adapter {
        content_type: "differential.diff",
        name: "Differential Diffs",
        description: "React to new diffs being uploaded, before writes occur.\nThese rules can \
                      reject diffs before they are written to permanent storage, to prevent \
                      users from accidentally uploading private keys or other sensitive \
                      information.",
        application: "PhabricatorDifferentialApplication",
        rule_types: &["global"],
    },
    Adapter {
        content_type: "differential",
        name: "Differential Revisions",
        description: "React to revisions being created or updated.\nRevision rules can send \
                      email, flag revisions, add reviewers, and run build plans.",
        application: "PhabricatorDifferentialApplication",
        rule_types: &["global", "personal"],
    },
    Adapter {
        content_type: "HeraldRuleAdapter",
        name: "Herald Rules",
        description: "React to Herald rules being created or updated.",
        application: APPLICATION,
        rule_types: &["global", "personal"],
    },
];

impl Adapter {
    /// `supportsRuleType()`.
    pub(crate) fn supports(&self, rule_type: &str) -> bool {
        self.rule_types.contains(&rule_type)
    }
}

/// `getEnabledAdapterMap()`: the adapters whose application the viewer
/// can use.
pub(crate) async fn enabled_adapters<S: WebState>(
    state: &S,
    viewer: &WebUser,
) -> Result<Vec<&'static Adapter>, PageError> {
    let mut enabled = Vec::new();
    for adapter in ADAPTERS {
        if harbormaster::can_use(state, viewer, adapter.application).await? {
            enabled.push(adapter);
        }
    }
    Ok(enabled)
}

/// An item of a menu.
pub(crate) struct MenuItem {
    name: &'static str,
    /// The paragraphs of the description, as HTML.
    description_html: Vec<String>,
    /// The link, or none for a disabled item.
    href: Option<String>,
}

#[derive(Template)]
#[template(path = "herald_new.html")]
pub(crate) struct MenuPage {
    shell: Shell,
    title: String,
    items: Vec<MenuItem>,
    /// "Back to Content Types".
    back: Option<&'static str>,
}

/// `phutil_escape_html_newlines()`.
fn lines_html(text: &str) -> String {
    text.split('\n')
        .map(crate::diff_html::escape)
        .collect::<Vec<_>>()
        .join("<br />")
}

/// `rawurlencode()`.
fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// `newAdapterMenu()`: the adapters, by name.
fn adapter_menu(mut adapters: Vec<&'static Adapter>) -> Vec<MenuItem> {
    adapters.sort_by_key(|adapter| adapter.name);
    adapters
        .into_iter()
        .map(|adapter| MenuItem {
            name: adapter.name,
            description_html: vec![lines_html(adapter.description)],
            href: Some(format!(
                "/herald/create/?adapter={}",
                encode(adapter.content_type)
            )),
        })
        .collect()
}

/// `newTypeMenu()`.
fn type_menu(adapter: &Adapter, can_global: bool) -> Vec<MenuItem> {
    let global_note = if can_global {
        "<em>You have permission to create and manage global rules.</em>"
    } else {
        "<em>You do not have permission to create or manage global rules.</em>"
    };
    let specs: [(&str, &str, Vec<String>, bool); 3] = [
        (
            "personal",
            "Personal Rule",
            vec![
                "Personal rules notify you about events. You own them, but they can only affect \
                 you. Personal rules only trigger for objects you have permission to see."
                    .to_owned(),
            ],
            true,
        ),
        (
            "object",
            "Object Rule",
            vec![
                "Object rules notify anyone about events. They are bound to an object (like a \
                 repository) and can only act on that object. You must be able to edit an \
                 object to create object rules for it. Other users who can edit the object can \
                 edit its rules."
                    .to_owned(),
            ],
            true,
        ),
        (
            "global",
            "Global Rule",
            vec![
                "Global rules notify anyone about events. Global rules can bypass access \
                 control policies and act on any object."
                    .to_owned(),
                global_note.to_owned(),
            ],
            can_global,
        ),
    ];
    specs
        .into_iter()
        .map(|(rule_type, name, mut description_html, enabled)| {
            let enabled = enabled && {
                let supported = adapter.supports(rule_type);
                if !supported {
                    description_html.push(
                        "<em>This rule type is not supported by the selected content type.</em>"
                            .to_owned(),
                    );
                }
                supported
            };
            MenuItem {
                name,
                description_html: vec![description_html.join("<br /><br />")],
                href: enabled.then(|| {
                    format!(
                        "/herald/create/?adapter={}&type={rule_type}",
                        encode(adapter.content_type)
                    )
                }),
            }
        })
        .collect()
}

/// Whether the viewer has `HeraldManageGlobalRulesCapability`.
pub(crate) async fn can_manage_global<S: WebState>(
    state: &S,
    viewer: &WebUser,
) -> Result<bool, PageError> {
    let policy = phab_app_herald::web::global_rules_policy(state.config());
    let subject = phab_policy::PolicySubject::new(None, phab_policy::ObjectKind::Other)
        .with_policy(phab_policy::Capability::EDIT, policy);
    let as_viewer = viewer.user.viewer();
    let facts = phab_policy::loader::load_facts(
        state.database(),
        state.config(),
        &as_viewer,
        &[&subject],
        &phab_policy::loader::NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    phab_policy::has_capability(&as_viewer, &subject, phab_policy::Capability::EDIT, &facts)
        .map_err(harbormaster::internal)
}

/// `GET|POST /herald/new/` and `/herald/create/`.
pub(crate) async fn new_rule<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let state = state.as_ref();
    let request = LoginRequest::new(method, uri.clone(), headers, &body);
    let adapters = enabled_adapters(state, &viewer).await?;
    let adapter = request
        .str("adapter")
        .filter(|adapter| !adapter.is_empty())
        .and_then(|key| adapters.iter().find(|adapter| adapter.content_type == key));
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    let Some(adapter) = adapter else {
        let title = "Create Herald Rule".to_owned();
        return Ok(Page(MenuPage {
            shell: Shell::for_user(state.config(), &viewer, &title, current),
            title,
            items: adapter_menu(adapters),
            back: None,
        })
        .into_response());
    };
    match request.str("type") {
        Some(rule_type @ ("global" | "personal")) => {
            let next = format!(
                "/herald/edit/?content_type={}&rule_type={rule_type}",
                encode(adapter.content_type)
            );
            Ok(request.redirect(state.config(), &next))
        }
        Some("object") => Err(PageError::Fallback("the object form of object rules")),
        _ => {
            let title = format!("Create Herald Rule: {}", adapter.name);
            let can_global = can_manage_global(state, &viewer).await?;
            Ok(Page(MenuPage {
                shell: Shell::for_user(state.config(), &viewer, &title, current),
                title,
                items: type_menu(adapter, can_global),
                back: Some("/herald/create/"),
            })
            .into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_read_like_php() {
        let menu = adapter_menu(ADAPTERS.iter().collect());
        let names: Vec<&str> = menu.iter().map(|item| item.name).collect();
        assert_eq!(names[0], "Calendar Events");
        assert_eq!(names[2], "Commit Hook: Commit Content");
        let revisions = menu
            .iter()
            .find(|item| item.name == "Differential Revisions")
            .unwrap();
        assert_eq!(
            revisions.href.as_deref(),
            Some("/herald/create/?adapter=differential")
        );
        assert!(revisions.description_html[0].contains("updated.<br />Revision"));
        let adapter = ADAPTERS
            .iter()
            .find(|a| a.content_type == "differential")
            .unwrap();
        let types = type_menu(adapter, false);
        assert_eq!(
            types[0].href.as_deref(),
            Some("/herald/create/?adapter=differential&type=personal")
        );
        assert_eq!(types[1].href, None);
        assert!(types[1].description_html[0].ends_with("content type.</em>"));
        assert_eq!(types[2].href, None);
        assert!(types[2].description_html[0].ends_with("global rules.</em>"));
        assert_eq!(encode("differential.diff"), "differential.diff");
    }
}
