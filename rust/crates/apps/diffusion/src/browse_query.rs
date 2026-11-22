//! `diffusion.browsequery` (`DiffusionBrowseQueryConduitAPIMethod`): the
//! entries of a Git directory (`git ls-tree`), or why there are none.
//!
//! Ported for Git: every parameter, submodules and their URIs included.
//! Left to PHP: Git failures PHP rethrows, and values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture, pager};
use phab_core::php as core_php;
use serde_json::{Map, Value, json};

use crate::drequest::DiffusionRequest;
use crate::git::{self, WorkingCopy};
use crate::php;
use crate::query::QueryMethod;

/// `DifferentialChangeType::FILE_DIRECTORY`.
const FILE_DIRECTORY: i64 = 4;
/// `DifferentialChangeType::FILE_SYMLINK`.
const FILE_SYMLINK: i64 = 5;
/// `DifferentialChangeType::FILE_NORMAL`.
const FILE_NORMAL: i64 = 7;
/// `DifferentialChangeType::FILE_SUBMODULE`.
const FILE_SUBMODULE: i64 = 8;

/// `DiffusionBrowseResultSet::toDictionary()`.
#[derive(Default)]
struct ResultSet {
    paths: Vec<Map<String, Value>>,
    is_valid: bool,
    reason: Option<&'static str>,
    existed_at: Option<String>,
    deleted_at: Option<String>,
}

impl ResultSet {
    fn empty(reason: &'static str) -> Self {
        Self {
            reason: Some(reason),
            ..Self::default()
        }
    }

    fn into_json(self) -> Value {
        json!({
            "paths": self.paths,
            "isValidResults": self.is_valid,
            "reasonForEmptyResultSet": self.reason,
            "existedAtCommit": self.existed_at,
            "deletedAtCommit": self.deleted_at,
        })
    }
}

/// `intval($mode, 8)`: the leading octal digits.
fn octal(mode: &str) -> i64 {
    mode.bytes()
        .take_while(|byte| (b'0'..=b'7').contains(byte))
        .fold(0_i64, |value, digit| {
            value
                .saturating_mul(8)
                .saturating_add(i64::from(digit - b'0'))
        })
}

/// `basename()` of a Git path.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(slash) => &trimmed[slash + 1..],
        None => trimmed,
    }
}

/// The `config -l` dictionary of `.gitmodules`.
fn parse_config(module_info: &str) -> Result<Map<String, Value>, MethodError> {
    let mut dict = Map::new();
    for line in php::trim(module_info).split('\n') {
        let Some((key, value)) = line.split_once('=') else {
            // PHP warns on list() of a missing value.
            return Err(MethodError::Unsupported("an unusual .gitmodules"));
        };
        dict.insert(key.to_owned(), json!(value));
    }
    Ok(dict)
}

/// The URIs of the submodules at these indexes of `paths`, from
/// `.gitmodules` (skipped submodules have none).
async fn add_submodule_uris(
    copy: &WorkingCopy,
    commit: &str,
    paths: &mut [Map<String, Value>],
    submodules: Vec<usize>,
) -> Result<(), MethodError> {
    let output = copy
        .exec(
            vec![
                "cat-file".to_owned(),
                "blob".to_owned(),
                "--".to_owned(),
                format!("{commit}:.gitmodules"),
            ],
            None,
        )
        .await?;
    if output.success() {
        let module_info = copy
            .execx(
                vec![
                    "config".to_owned(),
                    "-l".to_owned(),
                    "--file".to_owned(),
                    "-".to_owned(),
                    "--".to_owned(),
                ],
                Some(output.stdout),
            )
            .await?;
        let dict = parse_config(&php::text(module_info)?)?;
        for index in submodules {
            let Some(entry) = paths.get_mut(index) else {
                continue;
            };
            let full_path = entry["fullPath"].as_str().unwrap_or_default();
            if let Some(uri) = dict.get(&format!("submodule.{full_path}.url")) {
                entry.insert("externalURI".to_owned(), uri.clone());
            }
        }
    }
    Ok(())
}

/// The type of `path` at `commit` (`git cat-file -t`), or why there is
/// nothing to list.
async fn path_type(
    copy: &WorkingCopy,
    commit: &str,
    path: &str,
) -> Result<Result<String, ResultSet>, MethodError> {
    let output = copy
        .exec(
            vec![
                "cat-file".to_owned(),
                "-t".to_owned(),
                "--".to_owned(),
                format!("{commit}:{path}"),
            ],
            None,
        )
        .await?;
    if output.success() {
        Ok(Ok(php::trim(&php::text(output.stdout)?).to_owned()))
    } else {
        // A missing path, or a submodule.
        let tree = copy
            .exec(
                vec![
                    "ls-tree".to_owned(),
                    git::ref_selector(commit)?,
                    "--".to_owned(),
                    path.to_owned(),
                ],
                None,
            )
            .await?;
        if tree.success() {
            let mode = php::int_cast(Some(&json!(php::text(tree.stdout)?)))?;
            // PHP masks with the decimal 160000, not the octal mode.
            if mode & 0x0002_7100 != 0 {
                return Ok(Err(ResultSet::empty("is-submodule")));
            }
        }
        if !output.stderr.starts_with(b"fatal: Not a valid object name") {
            return Err(MethodError::Unsupported("a Git failure PHP rethrows"));
        }
        // The first commit is the one which deleted the path.
        let stdout = copy
            .execx(
                vec![
                    "log".to_owned(),
                    "-n2".to_owned(),
                    "--format=%H".to_owned(),
                    git::ref_selector(commit)?,
                    "--".to_owned(),
                    path.to_owned(),
                ],
                None,
            )
            .await?;
        let stdout = php::text(stdout)?;
        let stdout = php::trim(&stdout);
        if core_php::is_truthy_str(stdout) {
            let mut commits = stdout.split('\n');
            return Ok(Err(ResultSet {
                deleted_at: commits.next().map(str::to_owned),
                existed_at: commits.next().map(str::to_owned),
                ..ResultSet::empty("is-deleted")
            }));
        }
        Ok(Err(ResultSet::empty("nonexistent")))
    }
}

async fn browse(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
) -> Result<ResultSet, MethodError> {
    let copy = &drequest.copy;
    let path = php::nonempty_string(request.value("path"))?.filter(|path| path != "/");
    let commit = php::optional_string(request.value("commit"))?.unwrap_or_default();
    let offset = php::int_cast(request.value("offset"))?;
    let limit = php::int_cast(request.value("limit"))?;

    let path_type = match &path {
        // The root is always a tree.
        None => "tree".to_owned(),
        Some(path) => match path_type(copy, &commit, path).await? {
            Ok(path_type) => path_type,
            Err(result) => return Ok(result),
        },
    };
    if path_type == "blob" {
        return Ok(ResultSet::empty("is-file"));
    }
    let mut result = ResultSet {
        is_valid: true,
        ..ResultSet::default()
    };
    if request
        .value("needValidityOnly")
        .is_some_and(core_php::is_truthy)
    {
        return Ok(result);
    }

    let mut argv = vec![
        "ls-tree".to_owned(),
        "-z".to_owned(),
        "-l".to_owned(),
        git::ref_selector(&commit)?,
        "--".to_owned(),
    ];
    let path = path.map(|path| {
        if path_type == "tree" {
            format!("{}/", path.trim_end_matches('/'))
        } else {
            path.trim_end_matches('/').to_owned()
        }
    });
    argv.extend(path.clone());
    let stdout = php::text(copy.execx(argv, None).await?)?;
    let lines: Vec<&str> = if stdout.is_empty() || stdout == "0" {
        Vec::new()
    } else {
        php::rtrim(&stdout).split('\0').collect()
    };

    let mut count = 0;
    let mut submodules = Vec::new();
    for line in lines {
        let parts = php::split_whitespace(line, 5);
        let [mode, kind, hash, size, full_path] = parts.as_slice() else {
            return Err(pager::core_error(format!(
                "Expected \"<mode> <type> <hash> <size>\\t<name>\", for ls-tree of \"{commit}:{}\", \
                 got: {line}",
                path.as_deref().unwrap_or_default()
            )));
        };
        let file_type = match *kind {
            "tree" => FILE_DIRECTORY,
            "commit" => FILE_SUBMODULE,
            _ if octal(mode) & 0o120_000 == 0o120_000 => FILE_SYMLINK,
            _ => FILE_NORMAL,
        };
        let local_path = if path.is_none() {
            full_path
        } else {
            basename(full_path)
        };
        if count >= offset {
            if file_type == FILE_SUBMODULE {
                submodules.push(result.paths.len());
            }
            let mut entry = Map::new();
            entry.insert("fullPath".to_owned(), json!(full_path));
            entry.insert("path".to_owned(), json!(local_path));
            entry.insert("hash".to_owned(), json!(hash));
            entry.insert("fileType".to_owned(), json!(file_type));
            entry.insert("fileSize".to_owned(), json!(size));
            entry.insert("externalURI".to_owned(), Value::Null);
            entry.insert("lastModifiedCommit".to_owned(), Value::Null);
            entry.insert("lastCommitData".to_owned(), Value::Null);
            result.paths.push(entry);
        } else if file_type == FILE_SUBMODULE {
            // Skipped by the offset: PHP still looks its URI up.
            submodules.push(usize::MAX);
        }
        count += 1;
        if limit != 0 && count >= offset.saturating_add(limit) {
            break;
        }
    }

    if !submodules.is_empty() {
        add_submodule_uris(copy, &commit, &mut result.paths, submodules).await?;
    }
    Ok(result)
}

/// `diffusion.browsequery`.
pub struct BrowseQuery;

impl QueryMethod for BrowseQuery {
    fn name(&self) -> &'static str {
        "diffusion.browsequery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "path",
            "commit",
            "needValidityOnly",
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
        Box::pin(async move { Ok(browse(request, drequest).await?.into_json()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_modes_and_names() {
        assert_eq!(octal("120000"), 0o120_000);
        assert_eq!(octal("100644"), 0o100_644);
        assert_eq!(basename("a/b/c"), "c");
        assert_eq!(basename("c"), "c");
    }

    #[test]
    fn parses_module_config() {
        assert_eq!(
            parse_config("submodule.x.path=x\nsubmodule.x.url=https://e.com/x=y\n"),
            Ok(serde_json::from_value::<Map<String, Value>>(json!({
                "submodule.x.path": "x",
                "submodule.x.url": "https://e.com/x=y",
            }))
            .unwrap())
        );
        assert!(parse_config("submodule.x.active").is_err());
    }
}
