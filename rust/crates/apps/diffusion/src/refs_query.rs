//! `diffusion.refsquery` (`DiffusionRefsQueryConduitAPIMethod`): the refs
//! decorating a Git commit (`git log --format=%d`).
//!
//! Ported for Git: every parameter. Each `href` is a `PhutilURI` object,
//! which `json_encode()` turns into `{}`. Left to PHP: values of unusual
//! types.

use phab_conduit::{ApiRequest, MethodFuture};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::query::QueryMethod;
use crate::{git, php};

/// The refs of `%d` output, like ` (HEAD -> master, tag: v1)`.
fn parse_decorations(stdout: &str) -> Vec<String> {
    let refs = stdout.trim_matches(['(', ')', ' ', '\n']);
    if !phab_core::php::is_truthy_str(refs) {
        return Vec::new();
    }
    refs.split(',')
        .map(|reference| php::trim(reference).to_owned())
        .collect()
}

/// `diffusion.refsquery`.
pub struct RefsQuery;

impl QueryMethod for RefsQuery {
    fn name(&self) -> &'static str {
        "diffusion.refsquery"
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
            let commit = git::ref_selector(&php::string(request.value("commit"))?)?;
            let stdout = drequest
                .copy
                .execx(
                    vec![
                        "log".to_owned(),
                        "-n".to_owned(),
                        "1".to_owned(),
                        "--format=%d".to_owned(),
                        commit,
                        "--".to_owned(),
                    ],
                    None,
                )
                .await?;
            let refs = parse_decorations(&php::text(stdout)?);
            Ok(Value::Array(
                refs.into_iter()
                    .map(|reference| json!({"ref": reference, "href": {}}))
                    .collect(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decorations() {
        assert_eq!(
            parse_decorations(" (HEAD -> master, tag: v1.0, origin/master)\n"),
            ["HEAD -> master", "tag: v1.0", "origin/master"]
        );
        assert_eq!(parse_decorations("\n"), Vec::<String>::new());
    }
}
