//! Application visibility (`PhabricatorApplication::isClassInstalledForViewer`).

use phab_core::{Config, Phid, PhidType};
use std::collections::{HashMap, HashSet};

use crate::{Capability, ObjectKind, POLICY_ADMIN, POLICY_PUBLIC, POLICY_USER, PolicySubject};

use phab_core::php;

/// Applications whose `canUninstall()` returns false: always installed and
/// always visible.
const PERMANENT_APPLICATIONS: &[&str] = &[
    "PhabricatorApplicationsApplication",
    "PhabricatorAuditApplication",
    "PhabricatorAuthApplication",
    "PhabricatorCelerityApplication",
    "PhabricatorConduitApplication",
    "PhabricatorConfigApplication",
    "PhabricatorDaemonsApplication",
    "PhabricatorFeedApplication",
    "PhabricatorFilesApplication",
    "PhabricatorHelpApplication",
    "PhabricatorMetaMTAApplication",
    "PhabricatorPassphraseApplication",
    "PhabricatorPeopleApplication",
    "PhabricatorPolicyApplication",
    "PhabricatorSettingsApplication",
    "PhabricatorSpacesApplication",
    "PhabricatorSubscriptionsApplication",
    "PhabricatorSupportApplication",
    "PhabricatorSystemApplication",
    "PhabricatorTransactionsApplication",
    "PhabricatorTypeaheadApplication",
];

const PROTOTYPE_APPLICATIONS: &[&str] = &["PhabricatorCalendarApplication"];

/// `PhabricatorPHIDType::getPHIDTypeApplicationClass()` for the PHID types
/// the Rust services handle.
pub fn application_for_phid_type(phid_type: PhidType) -> Option<&'static str> {
    let class = match phid_type {
        PhidType::USER => "PhabricatorPeopleApplication",
        PhidType::PROJECT => "PhabricatorProjectApplication",
        PhidType::REVISION | PhidType::DIFF => "PhabricatorDifferentialApplication",
        PhidType::REPOSITORY | PhidType::COMMIT => "PhabricatorDiffusionApplication",
        PhidType::POLICY => "PhabricatorPolicyApplication",
        PhidType::SPACE => "PhabricatorSpacesApplication",
        PhidType::FILE => "PhabricatorFilesApplication",
        PhidType::TRANSACTION => "PhabricatorTransactionsApplication",
        _ => return None,
    };
    Some(class)
}

pub fn can_uninstall(class: &str) -> bool {
    !PERMANENT_APPLICATIONS.contains(&class)
}

/// The configuration which decides who can see applications.
#[derive(Debug, Clone, Default)]
pub struct ApplicationPolicies {
    show_prototypes: bool,
    uninstalled: HashSet<String>,
    locked: HashMap<String, String>,
    settings: HashMap<String, HashMap<String, String>>,
}

impl ApplicationPolicies {
    pub fn from_config(config: &Config) -> Self {
        let uninstalled = config
            .object("phabricator.uninstalled-applications")
            .map(|map| {
                map.iter()
                    .filter(|(_, value)| php::is_truthy(value))
                    .map(|(class, _)| class.clone())
                    .collect()
            })
            .unwrap_or_default();
        let locked = config
            .object("policy.locked")
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let settings = config
            .object("phabricator.application-settings")
            .map(|map| {
                map.iter()
                    .filter_map(|(application_phid, settings)| {
                        let policies = settings.get("policy")?.as_object()?;
                        let policies = policies
                            .iter()
                            .filter_map(|(capability, policy)| {
                                Some((capability.clone(), policy.as_str()?.to_owned()))
                            })
                            .collect();
                        Some((application_phid.clone(), policies))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            show_prototypes: config.truthy("phabricator.show-prototypes"),
            uninstalled,
            locked,
            settings,
        }
    }

    /// `PhabricatorApplication::isInstalled()`.
    pub fn is_installed(&self, class: &str) -> bool {
        if !can_uninstall(class) {
            return true;
        }
        if !self.show_prototypes && PROTOTYPE_APPLICATIONS.contains(&class) {
            return false;
        }
        !self.uninstalled.contains(class)
    }

    /// `PhabricatorApplication::getPolicy()` for view and edit.
    pub fn policy(&self, class: &str, capability: Capability, allow_public: bool) -> &str {
        let editable = capability != Capability::VIEW || can_uninstall(class);
        if editable {
            // `policy.locked` is keyed by the capability, as in PHP.
            if let Some(policy) = self.locked.get(capability.as_str()) {
                return policy;
            }
            let configured = self
                .settings
                .get(&format!("PHID-APPS-{class}"))
                .and_then(|policies| policies.get(capability.as_str()))
                .filter(|policy| !policy.is_empty());
            if let Some(policy) = configured {
                return policy;
            }
        }
        match capability {
            Capability::VIEW if allow_public => POLICY_PUBLIC,
            Capability::EDIT => POLICY_ADMIN,
            _ => POLICY_USER,
        }
    }

    /// The application as a policy object.
    pub fn subject(&self, class: &str, allow_public: bool) -> PolicySubject {
        PolicySubject::new(
            Some(Phid::new(format!("PHID-APPS-{class}"))),
            ObjectKind::Other,
        )
        .with_policy(
            Capability::VIEW,
            self.policy(class, Capability::VIEW, allow_public),
        )
        .with_policy(
            Capability::EDIT,
            self.policy(class, Capability::EDIT, allow_public),
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn policies(values: Value) -> ApplicationPolicies {
        let mut config = Config::default();
        if let Value::Object(map) = values {
            config.push_layer("test", map);
        }
        ApplicationPolicies::from_config(&config)
    }

    #[test]
    fn permanent_applications_ignore_uninstall_and_view_settings() {
        let policies = policies(json!({
            "phabricator.uninstalled-applications": {"PhabricatorPeopleApplication": true},
            "phabricator.application-settings": {
                "PHID-APPS-PhabricatorPeopleApplication": {"policy": {"view": "admin"}}
            },
        }));
        assert!(policies.is_installed("PhabricatorPeopleApplication"));
        assert_eq!(
            policies.policy("PhabricatorPeopleApplication", Capability::VIEW, false),
            "users"
        );
    }

    #[test]
    fn uninstalled_and_prototype_applications() {
        let policies = policies(json!({
            "phabricator.uninstalled-applications": {
                "PhabricatorDiffusionApplication": true,
                "PhabricatorProjectApplication": false,
            },
        }));
        assert!(!policies.is_installed("PhabricatorDiffusionApplication"));
        assert!(policies.is_installed("PhabricatorProjectApplication"));
        assert!(!policies.is_installed("PhabricatorCalendarApplication"));
    }

    #[test]
    fn configured_and_default_policies() {
        let policies = policies(json!({
            "phabricator.application-settings": {
                "PHID-APPS-PhabricatorDifferentialApplication": {"policy": {"view": "PHID-PROJ-x"}}
            },
        }));
        let differential = "PhabricatorDifferentialApplication";
        assert_eq!(
            policies.policy(differential, Capability::VIEW, false),
            "PHID-PROJ-x"
        );
        let diffusion = "PhabricatorDiffusionApplication";
        assert_eq!(policies.policy(diffusion, Capability::VIEW, true), "public");
        assert_eq!(policies.policy(diffusion, Capability::VIEW, false), "users");
        assert_eq!(policies.policy(diffusion, Capability::EDIT, true), "admin");
    }
}
