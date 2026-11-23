//! `diffusion.internal.ancestors` (`DiffusionInternalAncestorsConduitAPIMethod`):
//! the commits among `commits` which are ancestors of `ref`, read from a
//! `PhabricatorGitGraphStream`.
//!
//! Ported for Git: every parameter. Left to PHP: commits which are not
//! strings, a missing `ref` or `commits`, and values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::Value;

use crate::drequest::DiffusionRequest;
use crate::import::ImportError;
use crate::import::graph::GitGraphStream;
use crate::query::QueryMethod;
use crate::{git, php};

/// `diffusion.internal.ancestors`.
pub struct InternalAncestors;

/// The `commits` parameter: `foreach` over a list (or a dictionary's
/// values) of strings.
fn commits(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    let items: Vec<&Value> = match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(entries)) => entries.values().collect(),
        _ => return Err(MethodError::Unsupported("commits which are not a list")),
    };
    items
        .into_iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or(MethodError::Unsupported("commits which are not strings"))
}

impl QueryMethod for InternalAncestors {
    fn name(&self) -> &'static str {
        "diffusion.internal.ancestors"
    }

    fn params(&self) -> &'static [&'static str] {
        &["ref", "commits", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let commits = commits(request.value("commits"))?;
            let reference = php::string(request.value("ref"))?;
            // The constructor's gitsprintf() throws, outside the try block.
            let selector = git::ref_selector(&reference)?;
            // PHP's stream fails on a ref Git can't read before the loop's
            // try, quoting its command line: leave that answer to PHP.
            drequest
                .copy
                .execx(
                    vec![
                        "log".to_owned(),
                        "-n1".to_owned(),
                        "--format=%H".to_owned(),
                        selector,
                        "--".to_owned(),
                    ],
                    None,
                )
                .await?;
            let mut graph =
                GitGraphStream::new(&drequest.copy, Some(&reference)).map_err(method_error)?;
            let mut keep = Vec::new();
            for identifier in commits {
                match graph.commit_date(&identifier).await {
                    Ok(_) => keep.push(Value::String(identifier)),
                    // Not an ancestor, or `git log` failed: the exception is
                    // caught.
                    Err(ImportError::Failed(_)) => {}
                    Err(error) => return Err(method_error(error)),
                }
            }
            Ok(Value::Array(keep))
        })
    }
}

fn method_error(error: ImportError) -> MethodError {
    match error {
        ImportError::Unsupported(_) => {
            MethodError::Unsupported("reading the graph needs code which is not ported")
        }
        other => MethodError::internal(other),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn commits_are_lists_of_strings() {
        assert_eq!(
            commits(Some(&json!(["a", "b"]))),
            Ok(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(commits(Some(&json!({"x": "c"}))), Ok(vec!["c".to_owned()]));
        assert!(commits(None).is_err());
        assert!(commits(Some(&json!("a"))).is_err());
        assert!(commits(Some(&json!([1]))).is_err());
    }
}
