//! `diffusion.branchquery` (`DiffusionBranchQueryConduitAPIMethod`): the
//! branches of a Git repository, from `git for-each-ref`, or those
//! containing a commit, from `git branch --contains`.
//!
//! Ported for Git: every parameter. Left to PHP: branch filters with
//! regular expressions, and values of unusual types.

use phab_app_differential::repository::Repository;
use phab_conduit::{ApiRequest, MethodError, MethodFuture, pager};
use phab_core::php as core_php;
use phab_remarkup::preg::{self, Pattern};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::php::{self, Array, Key};
use crate::query::QueryMethod;
use crate::refs::{self, BRANCH};

static LOCAL_BRANCH: Pattern = Pattern::new(r"^[* ]*(\(no branch\)|\S+)\s+([a-z0-9]{40})");

/// `PhabricatorRepository::shouldTrackBranch()`: whether the branch
/// filter lets `branch` through.
pub fn tracks_branch(repository: &Repository, branch: &str) -> Result<bool, MethodError> {
    // (name, whether isset() sees it).
    let names: Vec<(String, bool)> = match repository.detail("branch-filter") {
        None | Some(Value::Null) => return Ok(true),
        Some(filter) if !core_php::is_truthy(filter) => return Ok(true),
        Some(Value::Object(filter)) => filter
            .iter()
            .map(|(name, value)| (name.clone(), !value.is_null()))
            .collect(),
        // A list's keys are its indexes.
        Some(Value::Array(filter)) => filter
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), !value.is_null()))
            .collect(),
        Some(_) => return Err(MethodError::Unsupported("an unusual branch filter")),
    };
    let is_regexp = |name: &str| name.starts_with("regexp(") && name.ends_with(')');
    if names.iter().any(|(name, _)| is_regexp(name)) {
        return Err(MethodError::Unsupported("branch filters with regexps"));
    }
    if is_regexp(branch) {
        return Ok(false);
    }
    Ok(names
        .iter()
        .any(|(name, set)| *set && Key::from_text(name) == Key::from_text(branch)))
}

/// `parseLocalBranchOutput()`: branch heads by name.
fn parse_local_branches(stdout: &str) -> Result<Array, MethodError> {
    let mut map = Array::new();
    // array_filter() drops "" and "0".
    for line in stdout
        .split('\n')
        .filter(|line| core_php::is_truthy_str(line))
    {
        let Some(groups) = preg::captures(&LOCAL_BRANCH, line.as_bytes()) else {
            return Err(pager::core_error(format!("Failed to parse {line}!")));
        };
        let branch = String::from_utf8_lossy(groups.get(1)).into_owned();
        let head = String::from_utf8_lossy(groups.get(2)).into_owned();
        if branch == "(no branch)" {
            continue;
        }
        map.set(Key::from_text(&branch), json!(head));
    }
    Ok(map)
}

/// Whether `idx($fields, 'closed') != $with_closed` keeps a Git branch,
/// which has no "closed" field: `null == $with_closed`.
fn null_equals(value: &Value) -> bool {
    match value {
        Value::String(text) => text.is_empty(),
        other => !core_php::is_truthy(other),
    }
}

/// `diffusion.branchquery`.
pub struct BranchQuery;

impl QueryMethod for BranchQuery {
    fn name(&self) -> &'static str {
        "diffusion.branchquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "closed",
            "limit",
            "offset",
            "contains",
            "patterns",
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
            let repository = &drequest.repository;
            let refs: Vec<Value> = match php::nonempty_string(request.value("contains"))? {
                Some(contains) => {
                    let patterns = match request.value("patterns") {
                        None | Some(Value::Null) => Vec::new(),
                        Some(Value::Array(patterns)) => patterns
                            .iter()
                            .map(|pattern| pattern.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                            .ok_or(MethodError::Unsupported("patterns which are not strings"))?,
                        Some(_) => {
                            return Err(MethodError::Unsupported("patterns which are not a list"));
                        }
                    };
                    let mut argv = vec![
                        "branch".to_owned(),
                        "--verbose".to_owned(),
                        "--no-abbrev".to_owned(),
                        "--contains".to_owned(),
                        contains,
                        "--".to_owned(),
                    ];
                    argv.extend(patterns);
                    let stdout = php::text(drequest.copy.execx(argv, None).await?)?;
                    parse_local_branches(&stdout)?
                        .iter()
                        .map(|(name, head)| {
                            json!({
                                "shortName": name.text(),
                                "commitIdentifier": head,
                                "refType": null,
                                "rawFields": [],
                            })
                        })
                        .collect()
                }
                None => refs::git_refs(&drequest.copy, &[BRANCH])
                    .await?
                    .iter()
                    .map(refs::GitRef::to_dictionary)
                    .collect(),
            };

            // processBranchRefs().
            let mut kept = Vec::new();
            for git_ref in refs {
                let name = git_ref["shortName"].as_str().unwrap_or_default();
                if tracks_branch(repository, name)? {
                    kept.push(git_ref);
                }
            }
            if request
                .value("closed")
                .is_some_and(|closed| !closed.is_null() && !null_equals(closed))
            {
                kept.clear();
            }
            let offset = request.value("offset");
            if offset.is_some_and(core_php::is_truthy) {
                let range = php::slice(kept.len(), php::int_param(offset, 0)?, None);
                kept = kept.drain(range).collect();
            }
            let limit = request.value("limit");
            if limit.is_some_and(core_php::is_truthy) {
                let range = php::slice(kept.len(), 0, Some(php::int_param(limit, 0)?));
                kept = kept.drain(range).collect();
            }
            Ok(Value::Array(kept))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_branches() {
        let head = "0123456789abcdef0123456789abcdef01234567";
        let output = format!("* master {head} Message\n  12 {head} M\n  (no branch) {head} x\n");
        assert_eq!(
            parse_local_branches(&output).map(Array::into_json),
            Ok(json!({"master": head, "12": head}))
        );
        assert!(parse_local_branches("+ wt 0123 x").is_err());
    }

    #[test]
    fn compares_closed_with_null() {
        assert!(null_equals(&json!(false)));
        assert!(null_equals(&json!(0)));
        assert!(null_equals(&json!("")));
        assert!(!null_equals(&json!("0")));
        assert!(!null_equals(&json!(true)));
    }
}
