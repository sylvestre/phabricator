//! `diffusion.searchquery` (`DiffusionSearchQueryConduitAPIMethod`): the
//! lines of a Git tree matching a pattern (`git grep`).
//!
//! Ported for Git: every parameter. Left to PHP: Git failures other than
//! "no match", and values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use phab_remarkup::preg::{self, Pattern};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::git::{self, LinesEnd};
use crate::php;
use crate::query::QueryMethod;

static MATCH: Pattern = Pattern::new(r"[^:]*:(.+)\x00(.+)\x00(.*)");
static BINARY: Pattern = Pattern::new(r"Binary file [^:]*:(.+) matches");

/// One line of `git grep --null -n` output.
fn parse_line(line: &[u8]) -> Result<Value, MethodError> {
    let text = |bytes: &[u8]| php::text(bytes.to_vec());
    if let Some(groups) = preg::captures(&MATCH, line) {
        return Ok(json!([
            text(groups.get(1))?,
            text(groups.get(2))?,
            text(groups.get(3))?
        ]));
    }
    if let Some(groups) = preg::captures(&BINARY, line) {
        return Ok(json!([text(groups.get(1))?, null, "Binary file"]));
    }
    Ok(json!([null, null, text(line)?]))
}

/// `diffusion.searchquery`.
pub struct SearchQuery;

impl QueryMethod for SearchQuery {
    fn name(&self) -> &'static str {
        "diffusion.searchquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "path",
            "commit",
            "grep",
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
            let path = drequest.path()?;
            let grep = php::string(request.value("grep"))?;
            // $offset + $limit, with null as 0.
            let stop = php::int_param(request.value("offset"), 0)?
                .checked_add(php::int_param(request.value("limit"), 0)?)
                .ok_or(MethodError::Unsupported("a huge limit"))?;
            // Git wants "." rather than an empty path.
            let path = if path.is_empty() {
                ".".to_owned()
            } else {
                path
            };
            let commit = drequest.stable_commit(request).await?;
            let argv = vec![
                "grep".to_owned(),
                "--extended-regexp".to_owned(),
                "--null".to_owned(),
                "-n".to_owned(),
                "--no-color".to_owned(),
                "-f".to_owned(),
                "-".to_owned(),
                git::ref_selector(&commit)?,
                "--".to_owned(),
                path,
            ];
            let (results, end) = drequest
                .copy
                .lines(
                    argv,
                    Some(grep.into_bytes()),
                    b'\n',
                    Vec::new(),
                    move |results: &mut Vec<Value>, line| {
                        results.push(parse_line(line)?);
                        Ok(i64::try_from(results.len()).unwrap_or(i64::MAX) < stop)
                    },
                )
                .await?;
            match end {
                LinesEnd::Stopped | LinesEnd::Exited(Some(0)) => {}
                // "git grep" exits with 1 when nothing matches.
                LinesEnd::Exited(Some(1)) => return Ok(Value::Array(Vec::new())),
                LinesEnd::Exited(_) => {
                    return Err(MethodError::Unsupported("a Git command which fails"));
                }
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
    fn parses_grep_lines() {
        assert_eq!(
            parse_line(b"abc:dir/a.txt\x0012\x00some: text").unwrap(),
            json!(["dir/a.txt", "12", "some: text"])
        );
        assert_eq!(
            parse_line(b"Binary file abc:x.bin matches").unwrap(),
            json!(["x.bin", null, "Binary file"])
        );
        assert_eq!(parse_line(b"odd").unwrap(), json!([null, null, "odd"]));
    }
}
