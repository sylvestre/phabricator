//! `diffusion.resolverefs` (`DiffusionResolveRefsConduitAPIMethod` with
//! `DiffusionLowLevelResolveRefsQuery`): branch and tag names, hashes and
//! other refs of a Git repository, resolved to commits.
//!
//! Ported for Git: every parameter, with PHP's quirks (see
//! [`refs::resolve()`]). Left to PHP: values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use phab_core::php as core_php;
use serde_json::Value;

use crate::drequest::DiffusionRequest;
use crate::query::QueryMethod;
use crate::refs;

/// A `list<string>` parameter.
fn strings(value: &Value) -> Result<Vec<String>, MethodError> {
    match value {
        Value::Array(list) => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or(MethodError::Unsupported(
                "refs or types which are not strings",
            )),
        _ => Err(MethodError::Unsupported(
            "refs or types which are not a list",
        )),
    }
}

/// `diffusion.resolverefs`.
pub struct ResolveRefs;

impl QueryMethod for ResolveRefs {
    fn name(&self) -> &'static str {
        "diffusion.resolverefs"
    }

    fn params(&self) -> &'static [&'static str] {
        &["refs", "types", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let references = strings(request.value("refs").unwrap_or(&Value::Null))?;
            let types = match request.value("types") {
                Some(types) if core_php::is_truthy(types) => Some(strings(types)?),
                _ => None,
            };
            let resolved = refs::resolve(&drequest.copy, &references, types.as_deref()).await?;
            Ok(refs::resolved_json(resolved))
        })
    }
}
