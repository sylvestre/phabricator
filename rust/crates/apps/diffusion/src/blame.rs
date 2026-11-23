//! `diffusion.blame` (`DiffusionBlameConduitAPIMethod` with
//! `DiffusionGitBlameQuery`): the commit of each line of Git paths, from
//! `git blame`.
//!
//! Ported for Git: every parameter but `timeout`, through
//! `diffusion.lastmodifiedquery` as PHP does (and with what it leaves to
//! PHP). PHP keeps blame in the immutable cache, keyed by the commit
//! which last changed the path; entries found in the database cache are
//! answered from it, but Rust doesn't write new ones. Left to PHP: a
//! `timeout` (whether Git beats it is up to the host) and values of
//! unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use phab_core::php as core_php;
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::last_modified_query::last_modified;
use crate::php::{self, Array, Key};
use crate::query::QueryMethod;
use crate::{cache, git};

/// `phutil_split_lines()`, keeping line endings.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// `resolveBlameFuture()`: the commit of each line.
fn parse_blame(stdout: &str) -> Vec<String> {
    split_lines(stdout)
        .into_iter()
        .map(|line| line.split(' ').next().unwrap_or_default().to_owned())
        .collect()
}

/// The cached blame of each key, from `PhabricatorKeyValueDatabaseCache`
/// (`readCacheData()`).
async fn read_cache(
    request: &ApiRequest<'_>,
    keys: &[(Key, String)],
) -> Result<Vec<(Key, Vec<String>)>, MethodError> {
    let names: Vec<String> = keys.iter().map(|(_, key)| key.clone()).collect();
    let mut cached = Vec::new();
    for ((path, _), data) in keys.iter().zip(cache::read(request, &names).await?) {
        let Some(data) = data else {
            continue;
        };
        let data = php::text(data)?;
        let Some((head, body)) = data.split_once('\n') else {
            return Err(MethodError::Unsupported("an unusual blame cache entry"));
        };
        if head == "raw" {
            cached.push((path.clone(), body.split('\n').map(str::to_owned).collect()));
        }
    }
    Ok(cached)
}

/// `diffusion.blame`.
pub struct Blame;

impl QueryMethod for Blame {
    fn name(&self) -> &'static str {
        "diffusion.blame"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "paths",
            "commit",
            "timeout",
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
            let paths: Vec<String> = match request.value("paths") {
                Some(Value::Array(paths)) => paths
                    .iter()
                    .map(|path| path.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or(MethodError::Unsupported("paths which are not strings"))?,
                _ => return Err(MethodError::Unsupported("paths which are not a list")),
            };
            if request.value("timeout").is_some_and(core_php::is_truthy) {
                return Err(MethodError::Unsupported("blame with a timeout"));
            }
            let commit = drequest.commit(request).await?;

            // loadCacheKeys(), through diffusion.lastmodifiedquery.
            let mut map: Vec<(Key, String)> = Vec::new();
            for path in &paths {
                let key = Key::from_text(path);
                if !map.iter().any(|(known, _)| *known == key) {
                    map.push((key, commit.clone()));
                }
            }
            let modified = last_modified(request, drequest, &map).await?;
            let repository_id = drequest.repository.id;
            let keys: Vec<(Key, String)> = map
                .iter()
                .filter_map(|(path, _)| {
                    let identifier = modified.get(path)?.as_str()?;
                    let hash = phab_core::hash::digest_for_index(path.text().as_bytes());
                    Some((
                        path.clone(),
                        format!("blame({repository_id}, {identifier}, {hash}, raw)"),
                    ))
                })
                .collect();

            let mut blame = Array::new();
            for (path, lines) in read_cache(request, &keys).await? {
                blame.set(path, json!(lines));
            }
            for path in &paths {
                let key = Key::from_text(path);
                if blame.get(&key).is_some() {
                    continue;
                }
                let output = drequest
                    .copy
                    .exec(
                        vec![
                            "--no-pager".to_owned(),
                            "blame".to_owned(),
                            "--root".to_owned(),
                            "-s".to_owned(),
                            "-l".to_owned(),
                            git::ref_selector(&commit)?,
                            "--".to_owned(),
                            path.clone(),
                        ],
                        None,
                    )
                    .await?;
                if output.success() {
                    blame.set(key, json!(parse_blame(&php::text(output.stdout)?)));
                }
            }
            Ok(blame.into_json())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blame() {
        assert_eq!(parse_blame("abc 1) x\ndef 2) y\n"), ["abc", "def"]);
        assert_eq!(parse_blame("abc 1) x"), ["abc"]);
        assert_eq!(parse_blame(""), [""]);
    }
}
