//! Policy names and explanations (`PhabricatorPolicy`), and
//! `/policy/explain/<phid>/<capability>/` (`PhabricatorPolicyExplainController`)
//! for applications. Other objects have special rules, extended policies
//! and spaces Rust doesn't describe, and go to PHP.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_core::Phid;
use phab_policy::loader::{self, NoExtendedObjects};
use phab_policy::{ApplicationPolicies, Capability};

use super::catalog::{self, Application};
use super::{Body, Link, PageBuilder, Section, SystemPage, no_query};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorPolicyApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorPolicyApplication";

/// A policy, as the pages show it.
#[derive(Debug, Clone)]
pub(crate) struct Described {
    /// `getShortName()`.
    pub(crate) short_name: String,
    /// `getIcon()`.
    pub(crate) icon: &'static str,
    /// `getPolicyExplanation()`.
    pub(crate) explanation: String,
    /// Custom policies have rules Rust doesn't show.
    pub(crate) is_custom: bool,
}

impl Described {
    /// `PhabricatorPolicyRef::newCapabilityLink()`: the policy's name,
    /// linking to its explanation.
    pub(crate) fn capability_link(&self, object: &str, capability: &str) -> String {
        format!(
            "<a href=\"/policy/explain/{}/{}/\"><span class=\"fa {}\" aria-hidden=\"true\">\
             </span> {}</a>",
            escape(object),
            escape(capability),
            self.icon,
            escape(&self.short_name)
        )
    }
}

/// `PhabricatorPolicyQuery` and `PhabricatorPolicy::getPolicyExplanation()`
/// of each of `identifiers`.
pub(crate) async fn describe<S: WebState>(
    state: &S,
    viewer: &WebUser,
    identifiers: &[String],
) -> Result<HashMap<String, Described>, PageError> {
    let objects: Vec<Phid> = identifiers
        .iter()
        .filter(|identifier| identifier.starts_with("PHID-"))
        .filter(|identifier| matches!(identifier.get(5..9), Some("PROJ" | "USER")))
        .map(Phid::new)
        .collect();
    let handles = if objects.is_empty() {
        HashMap::new()
    } else {
        harbormaster::load_handles(state, &Scope::new(state, viewer), &objects).await?
    };
    let mut described = HashMap::new();
    for identifier in identifiers {
        let (short_name, icon, explanation, is_custom) = match identifier.as_str() {
            "public" => (
                "Public".to_owned(),
                "fa-globe",
                "This object is public and can be viewed by anyone, even if they do not have \
                 an account on this server."
                    .to_owned(),
                false,
            ),
            "users" => (
                "All Users".to_owned(),
                "fa-users",
                "Logged in users can take this action.".to_owned(),
                false,
            ),
            "admin" => (
                "Administrators".to_owned(),
                "fa-eye",
                "Administrators can take this action.".to_owned(),
                false,
            ),
            "no-one" => (
                "No One".to_owned(),
                "fa-ban",
                "By default, no one can take this action.".to_owned(),
                false,
            ),
            other if other.get(5..9) == Some("PROJ") => {
                let handle = harbormaster::handle(&handles, &Phid::new(other));
                (
                    handle.name.clone(),
                    "fa-briefcase",
                    if handle.uri.is_some() {
                        format!(
                            "Members of the project \"{}\" can take this action.",
                            handle.full_name
                        )
                    } else {
                        "Members of a particular project can take this action. (You can not see \
                         this object, so the name of this project is restricted.)"
                            .to_owned()
                    },
                    false,
                )
            }
            other if other.get(5..9) == Some("USER") => {
                let handle = harbormaster::handle(&handles, &Phid::new(other));
                (
                    handle.full_name.clone(),
                    "fa-user",
                    format!("{} can take this action.", handle.full_name),
                    false,
                )
            }
            other if other.get(5..9) == Some("PLCY") => (
                "Custom Policy".to_owned(),
                "fa-certificate",
                "This object has a custom policy controlling who can take this action.".to_owned(),
                true,
            ),
            _ => return Err(PageError::Fallback("an unusual policy")),
        };
        described.insert(
            identifier.clone(),
            Described {
                short_name,
                icon,
                explanation,
                is_custom,
            },
        );
    }
    Ok(described)
}

/// Whether the viewer can edit an application (configure its policies).
pub(crate) async fn can_edit_application<S: WebState>(
    state: &S,
    viewer: &WebUser,
    application: &Application,
) -> Result<bool, PageError> {
    let config = state.config();
    let subject =
        ApplicationPolicies::from_config(config).subject(&application.class, config.allow_public());
    let as_viewer = viewer.user.viewer();
    let facts = loader::load_facts(
        state.database(),
        config,
        &as_viewer,
        &[&subject],
        &NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    phab_policy::filter::has_capability(&as_viewer, &subject, Capability::EDIT, &facts)
        .map_err(|error| PageError::Internal(Box::new(error)))
}

/// `PhabricatorPolicyCapability::getCapabilityName()`.
fn capability_name(application: &Application, capability: &str) -> String {
    match capability {
        "view" => "Can View".to_owned(),
        "edit" => "Can Edit".to_owned(),
        other => application
            .capabilities
            .iter()
            .find(|custom| custom.key == other)
            .and_then(|custom| custom.label.clone())
            .unwrap_or_else(|| other.to_owned()),
    }
}

/// `GET /policy/explain/<phid>/<capability>/`.
pub(crate) async fn explain<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((phid, capability)): Path<(String, String)>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    no_query(&uri)?;
    let state = state.as_ref();
    let Some(class) = phid.strip_prefix("PHID-APPS-") else {
        return Err(PageError::Fallback("the policies of an object not ported"));
    };
    // `PhabricatorObjectQuery`: applications the viewer can see.
    let application = catalog::application(class).ok_or(PageError::NotFound)?;
    if !viewer
        .can_view_applications(state, &[class])
        .await?
        .first()
        .copied()
        .unwrap_or(false)
    {
        return Err(PageError::NotFound);
    }
    if !super::applications::capabilities(application)
        .iter()
        .any(|(key, _)| *key == capability)
    {
        return Err(PageError::NotFound);
    }
    let config = state.config();
    let policies = ApplicationPolicies::from_config(config);
    let identifier = super::applications::application_policy(
        application,
        &policies,
        &capability,
        config.allow_public(),
    );
    let described = describe(state, &viewer, std::slice::from_ref(&identifier)).await?;
    let policy = &described[&identifier];
    if policy.is_custom {
        // `PhabricatorPolicyRulesView`.
        return Err(PageError::Fallback("the rules of a custom policy"));
    }
    let body = format!(
        "<p><strong>{}:</strong> {}</p><p>In detail, this means that these users can take \
         this action, provided they pass all of the checks described above first:</p>\
         <ul><li>{}</li></ul>",
        escape(&capability_name(application, &capability)),
        escape(&policy.short_name),
        escape(&policy.explanation)
    );
    let done = application
        .base_uri
        .clone()
        .unwrap_or_else(|| "/".to_owned());
    Ok(
        PageBuilder::new(format!("Policy Details: {}", application.name))
            .icon("fa-lock")
            .section(Section::new("Object Policy", Body::Html(body)))
            .side(Link::new("Done", done).icon("fa-check"))
            .build(state, &viewer, &uri),
    )
}
