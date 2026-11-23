//! What PHP's classes say about the applications and the Conduit methods,
//! from `catalog/*.json` (`rust/scripts/generate-catalogs.php`), which the
//! pages read instead of PHP's code.

use std::sync::OnceLock;

use serde::Deserialize;

/// A custom capability of an application (`getCustomCapabilities()`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CustomCapability {
    pub(crate) key: String,
    pub(crate) label: Option<String>,
    pub(crate) default: String,
    pub(crate) editable: bool,
}

/// A help article of an application.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HelpArticle {
    pub(crate) name: String,
    pub(crate) href: String,
}

/// A `PhabricatorApplication`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Application {
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) short_description: Option<String>,
    pub(crate) flavor_text: Option<String>,
    pub(crate) overview: Option<String>,
    pub(crate) icon: Option<String>,
    pub(crate) base_uri: Option<String>,
    pub(crate) group: String,
    pub(crate) launchable: bool,
    pub(crate) unlisted: bool,
    pub(crate) prototype: bool,
    pub(crate) can_uninstall: bool,
    pub(crate) first_party: bool,
    pub(crate) pinned_by_default: bool,
    pub(crate) pinned_by_default_for_admins: bool,
    pub(crate) capabilities: Vec<CustomCapability>,
    pub(crate) help_articles: Vec<HelpArticle>,
}

impl Application {
    /// `getPHID()`.
    pub(crate) fn phid(&self) -> String {
        format!("PHID-APPS-{}", self.class)
    }

    /// `isPinnedByDefault()`. Audit is pinned when the viewer can use
    /// Diffusion; the caller checks that.
    pub(crate) fn is_pinned_by_default(&self, is_admin: bool) -> bool {
        if is_admin {
            self.pinned_by_default_for_admins
        } else {
            self.pinned_by_default
        }
    }
}

/// Every application, in `getAllApplications()` order.
pub(crate) fn applications() -> &'static [Application] {
    static APPLICATIONS: OnceLock<Vec<Application>> = OnceLock::new();
    APPLICATIONS.get_or_init(|| {
        serde_json::from_str(include_str!("../../catalog/applications.json"))
            .expect("applications.json is a list of applications")
    })
}

/// The application `class`.
pub(crate) fn application(class: &str) -> Option<&'static Application> {
    applications()
        .iter()
        .find(|application| application.class == class)
}

/// `PhabricatorApplication::getApplicationGroups()`.
pub(crate) const GROUPS: [(&str, &str); 4] = [
    ("core", "Core Applications"),
    ("util", "Utilities"),
    ("admin", "Administration"),
    ("developer", "Developer Tools"),
];

/// A parameter of a method.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Param {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) param_type: Option<String>,
}

/// An error a method can return.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MethodError {
    pub(crate) code: String,
    pub(crate) meaning: Option<String>,
}

/// A `ConduitAPIMethod`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Method {
    pub(crate) name: String,
    pub(crate) application_name: String,
    pub(crate) application: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) status: String,
    pub(crate) status_description: Option<String>,
    pub(crate) return_type: Option<String>,
    pub(crate) params: Vec<Param>,
    pub(crate) errors: Vec<MethodError>,
    pub(crate) scope: String,
    pub(crate) internal: bool,
    /// How many documentation pages the method has (search and edit
    /// engines), or -1 when PHP needs a viewer to say.
    pub(crate) documentation_pages: i64,
}

/// Every method, in `PhutilClassMapQuery` order (`getSortOrder()`).
pub(crate) fn methods() -> &'static [Method] {
    static METHODS: OnceLock<Vec<Method>> = OnceLock::new();
    METHODS.get_or_init(|| {
        serde_json::from_str(include_str!("../../catalog/conduit.json"))
            .expect("conduit.json is a list of methods")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogs_load() {
        let differential = application("PhabricatorDifferentialApplication").unwrap();
        assert_eq!(differential.name, "Differential");
        assert!(differential.is_pinned_by_default(false));
        assert!(methods().iter().any(|method| method.name == "conduit.ping"));
    }
}
