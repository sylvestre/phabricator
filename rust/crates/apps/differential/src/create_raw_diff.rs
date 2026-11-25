//! `differential.createrawdiff` (`DifferentialCreateRawDiffConduitAPIMethod`):
//! a diff from the text of a Git diff.
//!
//! Ported: Git diffs ([`phab_diff::arcanist::parse_diff`]),
//! `repositoryPHID` and the global and user `viewPolicy` values, which
//! must let the actor view the diff. Left to PHP: other diff formats and
//! the diffs arcanist's parser rejects, object, project and custom view
//! policies, and what [`crate::diff_editor`] leaves to PHP.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, PhidType, php};
use phab_diff::arcanist::{self, ParseError};
use phab_policy::Viewer;
use serde_json::{Value, json};

use crate::diff_editor::{self, DiffFields, PlannedDiff};

/// How many changesets and hunks PHP accepts.
const RAW_LIMIT: usize = 10_000;

/// A generic PHP exception, which Conduit reports as `ERR-CONDUIT-CORE`.
fn exception(message: String) -> MethodError {
    MethodError::Conduit {
        code: "ERR-CONDUIT-CORE".to_owned(),
        info: Some(message),
    }
}

/// `differential.createrawdiff`.
pub struct CreateRawDiff;

impl CreateRawDiff {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let actor = diff_editor::actor_phid(request)?;
        let Some(Value::String(raw_diff)) = request.value("diff") else {
            return Err(MethodError::Unsupported("raw diffs which are not strings"));
        };
        let repository_phid = match request.value("repositoryPHID") {
            None | Some(Value::Null) => None,
            Some(Value::String(phid)) => Some(phid.clone()),
            Some(_) => {
                return Err(MethodError::Unsupported(
                    "repository PHIDs which are not strings",
                ));
            }
        };
        if let Some(phid) = repository_phid
            .as_deref()
            .filter(|phid| php::is_truthy_str(phid))
            && diff_editor::repository(request, phid).await?.is_none()
        {
            return Err(exception(format!("No such repository \"{phid}\"!")));
        }
        if repository_phid.as_ref().is_some_and(|phid| phid.len() > 64) {
            return Err(MethodError::Unsupported("values longer than their column"));
        }
        let changes = match arcanist::parse_diff(raw_diff) {
            Ok(changes) => changes,
            Err(ParseError::Empty) => {
                return Err(exception("Can't parse an empty diff!".to_owned()));
            }
            Err(ParseError::Unsupported(what)) => return Err(MethodError::Unsupported(what)),
        };
        let (changesets, line_count) = diff_editor::build_changesets(request, &changes).await?;
        let size: usize = changesets
            .iter()
            .map(|changeset| 1 + changeset.hunks.len())
            .sum();
        if size > RAW_LIMIT {
            return Err(exception(
                "The raw diff you have submitted is too large to parse (it affects more than \
                 10,000 paths and hunks)."
                    .to_owned(),
            ));
        }
        // Herald's rules are left to PHP whatever the policy says.
        diff_editor::check_herald(request).await?;
        let view_policy = view_policy(request.value("viewPolicy"), &request.viewer)?;
        let planned = PlannedDiff {
            changesets,
            line_count,
            fields: DiffFields {
                creation_method: Some("web".to_owned()),
                author_phid: actor,
                repository_phid,
                lint_status: diff_editor::STATUS_SKIP,
                unit_status: diff_editor::STATUS_SKIP,
                ..DiffFields::default()
            },
            view_policy,
            properties: Vec::new(),
        };
        let (id, phid) = diff_editor::save(request, &planned).await?;
        let uri = format!(
            "{}/differential/diff/{id}/",
            production_uri(request)?.trim_end_matches('/')
        );
        Ok(json!({"id": id, "phid": phid, "uri": uri}))
    }
}

/// PHP's error when the view policy excludes the actor.
const EXCLUDED: &str = "Validation errors:\n  - You can not select this view policy, because you \
                        would no longer be able to view the object.";

/// The policy of the `TYPE_VIEW_POLICY` transaction PHP adds when
/// `viewPolicy` is truthy, once `validatePolicyTransaction()` checked
/// that `viewer` can view the new diff with it. The diff has no author
/// yet when PHP validates it (`updateDiffFromDict()` sets it later), so
/// the author's automatic capability doesn't apply: the policy alone
/// decides (`PhabricatorPolicyFilter::checkCapability()`).
fn view_policy(value: Option<&Value>, viewer: &Viewer) -> Result<Option<String>, MethodError> {
    let policy = match value {
        None => return Ok(None),
        Some(value) if !php::is_truthy(value) => return Ok(None),
        Some(Value::String(policy)) => policy,
        Some(_) => {
            return Err(MethodError::Unsupported(
                "view policies which are not strings",
            ));
        }
    };
    if policy.starts_with("obj.") {
        return Err(MethodError::Unsupported("object view policies"));
    }
    let phid_type = Phid::new(policy.as_str()).phid_type();
    if phid_type == PhidType::PROJECT || phid_type == PhidType::POLICY {
        return Err(MethodError::Unsupported("project and custom view policies"));
    }
    // The viewer is logged in: "public" is "users" or itself, which both
    // let them in. Unknown policies reject.
    let allowed = match policy.as_str() {
        "public" | "users" => true,
        "admin" => viewer.is_admin,
        _ if phid_type == PhidType::USER => viewer
            .phid
            .as_ref()
            .is_some_and(|phid| phid.as_str() == policy),
        _ => false,
    };
    if !allowed {
        return Err(exception(EXCLUDED.to_owned()));
    }
    if policy.len() > 64 {
        return Err(MethodError::Unsupported("values longer than their column"));
    }
    Ok(Some(policy.clone()))
}

/// `PhabricatorEnv::getProductionURI()`'s base.
fn production_uri(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    let config = &request.context.config;
    match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => Ok(uri.to_owned()),
        None => Ok(config.base_uri()?.to_owned()),
    }
}

impl Method for CreateRawDiff {
    fn name(&self) -> &'static str {
        "differential.createrawdiff"
    }

    fn params(&self) -> &'static [&'static str] {
        &["diff", "repositoryPHID", "viewPolicy"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_policies_must_let_the_actor_in() {
        let bob = Viewer::user(Phid::new("PHID-USER-bobbobbobbobbobbobbo"), false);
        let admin = Viewer::user(Phid::new("PHID-USER-adminadminadminadmi"), true);
        let read = |value: Value, viewer: &Viewer| view_policy(Some(&value), viewer);
        let set = |policy: &str| Some(Some(policy.to_owned()));
        assert_eq!(view_policy(None, &bob).ok(), Some(None));
        assert_eq!(read(json!(""), &bob).ok(), Some(None));
        assert_eq!(read(json!("0"), &bob).ok(), Some(None));
        for policy in ["users", "public", "PHID-USER-bobbobbobbobbobbobbo"] {
            assert_eq!(read(json!(policy), &bob).ok(), set(policy));
        }
        assert_eq!(read(json!("admin"), &admin).ok(), set("admin"));
        for policy in ["admin", "no-one", "PHID-USER-someoneelse000000", "nonsense"] {
            assert!(
                matches!(
                    read(json!(policy), &bob),
                    Err(MethodError::Conduit { info: Some(ref info), .. }) if info == EXCLUDED
                ),
                "{policy}"
            );
        }
        for policy in [
            json!("obj.differential.author"),
            json!("PHID-PLCY-abcdefghijklmnopqrst"),
            json!("PHID-PROJ-abcdefghijklmnopqrst"),
            json!(["users"]),
        ] {
            assert!(matches!(
                read(policy, &bob),
                Err(MethodError::Unsupported(_))
            ));
        }
    }
}
