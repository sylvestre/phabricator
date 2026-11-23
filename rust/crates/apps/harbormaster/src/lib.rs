//! Harbormaster (`PhabricatorHarbormasterApplication`): the read-only
//! Conduit methods, `harbormaster.*.search` and the legacy
//! `harbormaster.querybuildables`, `harbormaster.querybuilds` and
//! `harbormaster.queryautotargets`, and the writes of build systems:
//! `harbormaster.sendmessage` and `harbormaster.createartifact`.
//!
//! Harbormaster objects inherit their policies along a chain: a buildable
//! from the object it builds (a diff or a commit), a build from its
//! buildable, a target from its build, artifacts and logs from their
//! target; a step from its build plan, which has policies of its own.
//! `harbormaster.buildplan.edit` edits plans ([`plan_edit`]); the other
//! `*.edit` methods stay in PHP: `harbormaster.build.edit` and
//! `harbormaster.buildable.edit` have no field but `mfa` (their commands
//! are messages, sent by `sendmessage`), and the fields of
//! `harbormaster.step.edit` are the custom fields of the step
//! implementations (`core:customfield`). The write methods queue the
//! build workers' tasks, which [`engine`] runs. [`web`] is what the web
//! pages read.

pub mod artifact;
pub mod autotargets;
pub mod build;
pub mod buildable;
mod constraints;
pub mod engine;
pub mod message;
pub mod plan;
pub mod plan_edit;
mod query;
mod storage;
pub mod target;
mod visibility;
pub mod web;
mod writes;

use phab_conduit::{ApiRequest, Application, MethodError, Registry};
use phab_core::Config;
use phab_core::phid::phid_type_of;
use phab_policy::{POLICY_PUBLIC, POLICY_USER};
use serde_json::{Map, Value, json};

/// `PhabricatorHarbormasterApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorHarbormasterApplication",
    name: "Harbormaster",
};

/// Registers the Harbormaster methods.
pub fn register(registry: &mut Registry) {
    registry.register(buildable::BuildableSearch);
    registry.register(buildable::QueryBuildables);
    registry.register(build::BuildSearch);
    registry.register(build::QueryBuilds);
    registry.register(plan::BuildPlanSearch);
    registry.register(plan::StepSearch);
    registry.register(plan_edit::BuildPlanEdit);
    registry.register(target::TargetSearch);
    registry.register(target::ArtifactSearch);
    registry.register(target::LogSearch);
    registry.register(autotargets::QueryAutotargets);
    registry.register(message::SendMessage);
    registry.register(artifact::CreateArtifact);
}

/// The parameters of every `*.search` method
/// (`PhabricatorSearchEngineAPIMethod::defineParamTypes()`).
const SEARCH_PARAMS: &[&str] = &[
    "queryKey",
    "constraints",
    "attachments",
    "order",
    "before",
    "after",
    "limit",
];

/// `PhabricatorPolicies::getMostOpenPolicy()`.
fn most_open(config: &Config) -> &'static str {
    if config.allow_public() {
        POLICY_PUBLIC
    } else {
        POLICY_USER
    }
}

/// `PhabricatorEnv::getProductionURI()` of a path.
fn production_uri(request: &ApiRequest<'_>, path: &str) -> Result<String, MethodError> {
    let config = &request.context.config;
    let base = match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => uri,
        None => config.base_uri().map_err(MethodError::internal)?,
    };
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// `PhabricatorEnv::getURI()` of a path.
fn uri(request: &ApiRequest<'_>, path: &str) -> Result<String, MethodError> {
    request
        .context
        .config
        .uri(path)
        .map_err(MethodError::internal)
}

/// One result of a search (`buildConduitResponse()`): `fields` are the
/// object's own, followed by `dateCreated` and `dateModified`
/// (`PhabricatorLiskSearchEngineExtension`) and `policy`
/// (`PhabricatorPolicySearchEngineExtension`).
fn search_result(
    id: i64,
    phid: &str,
    mut fields: Map<String, Value>,
    dates: (i64, i64),
    policy: Value,
    attachments: Map<String, Value>,
) -> Value {
    fields.insert("dateCreated".to_owned(), json!(dates.0));
    fields.insert("dateModified".to_owned(), json!(dates.1));
    fields.insert("policy".to_owned(), policy);
    let mut result = Map::new();
    result.insert("id".to_owned(), json!(id));
    result.insert("type".to_owned(), json!(phid_type_of(phid).as_str()));
    result.insert("phid".to_owned(), json!(phid));
    result.insert("fields".to_owned(), Value::Object(fields));
    result.insert("attachments".to_owned(), Value::Object(attachments));
    Value::Object(result)
}

/// `HarbormasterBuildStepImplementation::getImplementation()`: the
/// implementations of this install, by class, with `getName()` and
/// `getBuildStepAutotargetStepKey()`.
fn step_implementation(class: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match class {
        "HarbormasterAbortOlderBuildsBuildStepImplementation" => ("Abort Older Builds", None),
        "HarbormasterArcLintBuildStepImplementation" => {
            ("Arcanist Lint Results", Some(autotargets::LINT_KEY))
        }
        "HarbormasterArcUnitBuildStepImplementation" => {
            ("Arcanist Unit Results", Some(autotargets::UNIT_KEY))
        }
        "HarbormasterHTTPRequestBuildStepImplementation" => ("Make HTTP Request", None),
        "HarbormasterWaitForPreviousBuildStepImplementation" => {
            ("Wait for Previous Commits to Build", None)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implementations() {
        assert_eq!(
            step_implementation("HarbormasterArcUnitBuildStepImplementation"),
            Some(("Arcanist Unit Results", Some("arcanist.unit")))
        );
        assert_eq!(step_implementation("Nope"), None);
    }
}
