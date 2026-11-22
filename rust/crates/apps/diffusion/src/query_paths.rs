//! `diffusion.querypaths` (`DiffusionQueryPathsConduitAPIMethod`): the
//! files of a Git tree (`git ls-tree -r`), optionally matching a regular
//! expression.
//!
//! Ported for Git: every parameter. Left to PHP: patterns which PHP
//! would refuse or warn about (unbalanced parentheses, invalid
//! expressions), Git failures, and values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::git::{self, LinesEnd};
use crate::php;
use crate::query::QueryMethod;

/// `'('.$pattern.')'` as `preg_match()` reads it: the delimiters must be
/// the outer parentheses (PHP closes the expression at the parenthesis
/// matching the first one, skipping escaped characters), with no flags.
fn compile_pattern(pattern: &str) -> Result<pcre2::bytes::Regex, MethodError> {
    let regex = format!("({pattern})");
    let bytes = regex.as_bytes();
    let mut depth = 1;
    let mut index = 1;
    let mut end = None;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 1 < bytes.len() => index += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(index);
                    break;
                }
            }
            b'(' => depth += 1,
            _ => {}
        }
        index += 1;
    }
    if end != Some(bytes.len() - 1) {
        return Err(MethodError::Unsupported("a pattern PHP can't read"));
    }
    phab_remarkup::preg::compile_php(bytes)
        .ok_or(MethodError::Unsupported("a pattern PHP can't compile"))
}

/// `diffusion.querypaths`.
pub struct QueryPaths;

impl QueryMethod for QueryPaths {
    fn name(&self) -> &'static str {
        "diffusion.querypaths"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "path",
            "commit",
            "pattern",
            "limit",
            "offset",
            "callsign",
            "repository",
            "branch",
        ]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            // Git wants "." rather than an empty path.
            let path = Some(drequest.path()?)
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| ".".to_owned());
            let commit = git::ref_selector(&php::string(request.value("commit"))?)?;
            // filterResults().
            let pattern = php::nonempty_string(request.value("pattern"))?
                .map(|pattern| compile_pattern(&pattern))
                .transpose()?;
            let limit = php::int_cast(request.value("limit"))?;
            let offset = php::int_cast(request.value("offset"))?;
            let stop = offset.saturating_add(limit);
            let argv = vec![
                "ls-tree".to_owned(),
                "--name-only".to_owned(),
                "-r".to_owned(),
                "-z".to_owned(),
                commit,
                "--".to_owned(),
                path,
            ];
            let (results, end) = drequest
                .copy
                .lines(
                    argv,
                    None,
                    b'\0',
                    Vec::new(),
                    move |results: &mut Vec<Value>, line| {
                        if let Some(pattern) = &pattern
                            && !pattern.is_match(line).unwrap_or(false)
                        {
                            return Ok(true);
                        }
                        results.push(json!(php::text(line.to_vec())?));
                        let count = i64::try_from(results.len()).unwrap_or(i64::MAX);
                        Ok(limit == 0 || count < stop)
                    },
                )
                .await?;
            if !matches!(end, LinesEnd::Stopped | LinesEnd::Exited(Some(0))) {
                return Err(MethodError::Unsupported("a Git command which fails"));
            }
            let range = php::slice(
                results.len(),
                php::int_param(request.value("offset"), 0)?,
                None,
            );
            Ok(Value::Array(results[range].to_vec()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_patterns_like_php() {
        assert!(compile_pattern(r"\.txt$").is_ok());
        assert!(compile_pattern("(a|b)c").is_ok());
        // PHP closes the expression early: "b" is read as a flag.
        assert!(compile_pattern("a)b(").is_err());
        assert!(compile_pattern("[)]").is_err());
        assert!(compile_pattern(r"\)").is_ok());
    }
}
