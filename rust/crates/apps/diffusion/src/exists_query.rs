//! `diffusion.existsquery` (`DiffusionExistsQueryConduitAPIMethod`):
//! whether `git cat-file -t` finds an object.
//!
//! Ported for Git: every parameter. Left to PHP: values of unusual types.

use phab_conduit::{ApiRequest, MethodFuture};
use serde_json::json;

use crate::drequest::DiffusionRequest;
use crate::php;
use crate::query::QueryMethod;

/// `diffusion.existsquery`.
pub struct ExistsQuery;

impl QueryMethod for ExistsQuery {
    fn name(&self) -> &'static str {
        "diffusion.existsquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &["commit", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let commit = php::shell_string(request.value("commit"))?;
            let output = drequest
                .copy
                .exec(
                    vec![
                        "cat-file".to_owned(),
                        "-t".to_owned(),
                        "--".to_owned(),
                        commit,
                    ],
                    None,
                )
                .await?;
            Ok(json!(output.success()))
        })
    }
}
