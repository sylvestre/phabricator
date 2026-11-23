//! The refs of a Git repository: `DiffusionLowLevelGitRefQuery` (`git
//! for-each-ref`), `DiffusionLowLevelResolveRefsQuery` (`git cat-file
//! --batch-check`) and `DiffusionCachedResolveRefsQuery` (the ref cursors
//! and commits in the database).

use indexmap::IndexMap;
use phab_app_differential::repository::Repository;
use phab_conduit::{MethodError, pager};
use phab_db::{Database, rows};
use phab_remarkup::preg::{self, Pattern};
use serde_json::{Map, Value, json};

use crate::git::WorkingCopy;
use crate::php::{self, Array, Key};

/// `PhabricatorRepositoryRefCursor::TYPE_BRANCH`.
pub const BRANCH: &str = "branch";
/// `PhabricatorRepositoryRefCursor::TYPE_TAG`.
pub const TAG: &str = "tag";
/// `PhabricatorRepositoryRefCursor::TYPE_REF`: any other ref, named in
/// full.
pub const REF: &str = "ref";

const BRANCH_PREFIX: &str = "refs/heads/";
const TAG_PREFIX: &str = "refs/tags/";

/// The `--format` fields, in order.
const FIELDS: [&str; 7] = [
    "objectname",
    "objecttype",
    "refname",
    "*objectname",
    "*objecttype",
    "subject",
    "creator",
];

static CREATOR: Pattern = Pattern::new(r"^(.*) ([0-9]+) ([0-9+-]+)$");

/// A ref `git for-each-ref` lists (`DiffusionRepositoryRef`).
#[derive(Debug, Clone, PartialEq)]
pub struct GitRef {
    /// The name without its `refs/heads/` or `refs/tags/` prefix.
    pub short_name: String,
    /// `branch` or `tag`.
    pub ref_type: &'static str,
    /// The commit (for annotated tags, the commit the tag points at).
    pub commit: String,
    /// The fields of the format, then `author` and `epoch`.
    pub fields: Map<String, Value>,
}

impl GitRef {
    /// A raw field, `None` when it is null.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }

    /// `toDictionary()`.
    pub fn to_dictionary(&self) -> Value {
        json!({
            "shortName": self.short_name,
            "commitIdentifier": self.commit,
            "refType": self.ref_type,
            "rawFields": self.fields,
        })
    }
}

/// `DiffusionLowLevelGitRefQuery` with `withRefTypes()` of branches, tags
/// and other refs ([`REF`], which refs under `refs/remotes/` are not).
pub async fn git_refs(copy: &WorkingCopy, types: &[&str]) -> Result<Vec<GitRef>, MethodError> {
    let prefix = match types {
        [only] if *only == BRANCH => BRANCH_PREFIX,
        [only] if *only == TAG => TAG_PREFIX,
        _ => "refs/",
    };
    let format = FIELDS
        .iter()
        .map(|field| format!("%({field})"))
        .collect::<Vec<_>>()
        .join("%01");
    let stdout = copy
        .execx(
            vec![
                "for-each-ref".to_owned(),
                "--sort=-creatordate".to_owned(),
                format!("--format={format}"),
                "--".to_owned(),
                prefix.to_owned(),
            ],
            None,
        )
        .await?;
    let stdout = php::text(stdout)?;
    let stdout = php::rtrim(&stdout);
    if stdout.is_empty() {
        return Ok(Vec::new());
    }
    let mut refs = Vec::new();
    for line in stdout.split('\n') {
        let parts: Vec<&str> = line.splitn(FIELDS.len(), '\u{1}').collect();
        if parts.len() < FIELDS.len() {
            // PHP would pass a null "creator" to preg_match().
            return Err(MethodError::Unsupported("an unusual for-each-ref line"));
        }
        let mut fields: Map<String, Value> = FIELDS
            .iter()
            .zip(&parts)
            .map(|(field, part)| ((*field).to_owned(), json!(part)))
            .collect();
        let refname = parts[2];
        let (short, ref_type) = if let Some(short) = refname.strip_prefix(BRANCH_PREFIX) {
            (short, BRANCH)
        } else if let Some(short) = refname.strip_prefix(TAG_PREFIX) {
            (short, TAG)
        } else if refname.starts_with("refs/remotes/") {
            // The remotes of the remote a repository observes.
            continue;
        } else {
            (refname, REF)
        };
        if !types.contains(&ref_type) || short == "HEAD" {
            continue;
        }
        let (author, epoch) = match preg::captures(&CREATOR, parts[6].as_bytes()) {
            Some(groups) => {
                let digits = String::from_utf8_lossy(groups.get(2)).into_owned();
                (
                    json!(String::from_utf8_lossy(groups.get(1))),
                    json!(digits.parse::<i64>().unwrap_or(i64::MAX)),
                )
            }
            None => (Value::Null, Value::Null),
        };
        fields.insert("author".to_owned(), author);
        fields.insert("epoch".to_owned(), epoch);
        // nonempty(): the peeled object of an annotated tag.
        let commit = if phab_core::php::is_truthy_str(parts[3]) {
            parts[3]
        } else {
            parts[0]
        };
        refs.push(GitRef {
            short_name: short.to_owned(),
            ref_type,
            commit: commit.to_owned(),
            fields,
        });
    }
    Ok(refs)
}

/// What a ref resolves to: lists of `{type, identifier, ...}` by ref,
/// with the holes `unset()` leaves in them.
pub type Resolved = IndexMap<Key, Array>;

/// The answer of `diffusion.resolverefs` for `resolved`.
pub fn resolved_json(resolved: Resolved) -> Value {
    resolved
        .into_iter()
        .map(|(key, matches)| (key, matches.into_json()))
        .collect::<Array>()
        .into_json()
}

fn add(resolved: &mut Resolved, key: Key, info: Value) {
    resolved.entry(key).or_default().push(info);
}

/// `filterRefsByType()`.
pub fn filter_by_type(resolved: &mut Resolved, types: &[String]) {
    for matches in resolved.values_mut() {
        // empty($type_map[$type]): a wanted type which is not "" or "0".
        matches.retain(|_, info| {
            info.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    phab_core::php::is_truthy_str(kind) && types.iter().any(|wanted| wanted == kind)
                })
        });
    }
    resolved.retain(|_, matches| !matches.is_empty());
}

fn is_full_hash(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'f' | b'0'..=b'9'))
}

/// `DiffusionLowLevelResolveRefsQuery::resolveGitRefs()`, then the type
/// filter. Like PHP, refs which don't resolve as branch or tag names
/// discard the names which did, and commits take the identifier of the
/// last line `git cat-file` printed.
pub async fn resolve(
    copy: &WorkingCopy,
    refs: &[String],
    types: Option<&[String]>,
) -> Result<Resolved, MethodError> {
    if refs.is_empty() {
        return Ok(Resolved::new());
    }
    // array_fuse().
    let mut unresolved: IndexMap<Key, String> = IndexMap::new();
    for reference in refs {
        unresolved.insert(Key::from_text(reference), reference.clone());
    }
    let mut results = Resolved::new();
    let possible: Vec<(Key, String)> = unresolved
        .iter()
        .filter(|(_, reference)| !is_full_hash(reference))
        .map(|(key, reference)| (key.clone(), reference.clone()))
        .collect();
    if !possible.is_empty() {
        let mut by_name: IndexMap<Key, Vec<GitRef>> = IndexMap::new();
        for found in git_refs(copy, &[BRANCH, TAG]).await? {
            by_name
                .entry(Key::from_text(&found.short_name))
                .or_default()
                .push(found);
        }
        for (key, _) in possible {
            let Some(found) = by_name.get(&key) else {
                continue;
            };
            for git_ref in found {
                let is_tag = git_ref
                    .field("refname")
                    .unwrap_or_default()
                    .starts_with(TAG_PREFIX);
                let mut info = Map::new();
                info.insert("type".to_owned(), json!(if is_tag { TAG } else { BRANCH }));
                info.insert("identifier".to_owned(), json!(git_ref.commit));
                if is_tag
                    && let Some(alternate) = git_ref
                        .field("objectname")
                        .filter(|name| phab_core::php::is_truthy_str(name))
                {
                    info.insert("alternate".to_owned(), json!(alternate));
                }
                add(&mut results, key.clone(), Value::Object(info));
            }
            unresolved.shift_remove(&key);
        }
    }
    if !unresolved.is_empty() {
        results = resolve_objects(copy, &unresolved).await?;
    }
    if let Some(types) = types {
        filter_by_type(&mut results, types);
    }
    Ok(results)
}

/// The `git cat-file --batch-check` half of `resolveGitRefs()`.
async fn resolve_objects(
    copy: &WorkingCopy,
    unresolved: &IndexMap<Key, String>,
) -> Result<Resolved, MethodError> {
    let input = unresolved.values().cloned().collect::<Vec<_>>().join("\n");
    let stdout = copy
        .execx(
            vec!["cat-file".to_owned(), "--batch-check".to_owned()],
            Some(input.into_bytes()),
        )
        .await?;
    let stdout = php::text(stdout)?;
    let lines: Vec<&str> = stdout.trim_end_matches('\n').split('\n').collect();
    if lines.len() != unresolved.len() {
        return Err(pager::core_error(
            "Unexpected line count from `git cat-file`!".to_owned(),
        ));
    }
    // The variables PHP's loops share.
    let mut identifier = String::new();
    let mut hits = Vec::new();
    let mut has_tags = false;
    for (key, line) in unresolved.keys().zip(lines) {
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() < 2 {
            return Err(pager::core_error(format!(
                "Failed to parse `git cat-file` output: {line}"
            )));
        }
        identifier = parts[0].to_owned();
        let kind = parts[1];
        match kind {
            "missing" => continue,
            "commit" => {}
            "tag" => has_tags = true,
            _ => {
                return Err(pager::core_error(format!(
                    "Unexpected object type from `git cat-file`: {line}"
                )));
            }
        }
        hits.push((key.clone(), kind.to_owned()));
    }
    let mut tag_map: IndexMap<Key, String> = IndexMap::new();
    if has_tags {
        for tag in git_refs(copy, &[TAG]).await? {
            tag_map.insert(Key::from_text(&tag.short_name), tag.commit);
        }
    }
    let mut results = Resolved::new();
    for (key, kind) in hits {
        let mut alternate = None;
        if kind == TAG
            && let Some(tag_identifier) = tag_map.get(&key)
        {
            alternate = Some(identifier.clone());
            identifier.clone_from(tag_identifier);
        }
        let mut info = Map::new();
        info.insert("type".to_owned(), json!(kind));
        info.insert("identifier".to_owned(), json!(identifier));
        if let Some(alternate) = alternate {
            info.insert("alternate".to_owned(), json!(alternate));
        }
        add(&mut results, key, Value::Object(info));
    }
    Ok(results)
}

/// `/^[a-f0-9]{4,}$/`, whose `$` also matches before a final newline.
fn looks_like_hash(reference: &str) -> bool {
    let hash = reference.strip_suffix('\n').unwrap_or(reference);
    hash.len() >= 4
        && hash
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'f' | b'0'..=b'9'))
}

/// `DiffusionCachedResolveRefsQuery` for Git: commit hash prefixes from
/// the commits table, branch and tag names from the ref cursors.
pub async fn resolve_cached(
    database: &Database,
    repository: &Repository,
    refs: &[String],
    types: Option<&[String]>,
) -> Result<Resolved, MethodError> {
    let mut results = Resolved::new();
    if refs.is_empty() {
        return Ok(results);
    }
    let prefixes: Vec<&String> = refs
        .iter()
        .filter(|reference| looks_like_hash(reference))
        .collect();
    if !prefixes.is_empty() {
        let sql = format!(
            "SELECT commitIdentifier FROM {} WHERE repositoryID = ? AND ({})",
            database.table("repository", "repository_commit"),
            vec!["(commitIdentifier LIKE CONCAT(?, '%'))"; prefixes.len()].join(" OR ")
        );
        let mut query = phab_db::sqlx::query(&sql).bind(repository.id);
        for prefix in &prefixes {
            query = query.bind(prefix.as_str());
        }
        for row in query.fetch_all(database.pool()).await? {
            let hash = rows::text(&row, "commitIdentifier")?;
            for reference in refs {
                if hash.starts_with(reference.as_str()) {
                    add(
                        &mut results,
                        Key::from_text(reference),
                        json!({"type": "commit", "identifier": hash}),
                    );
                }
            }
        }
    }
    let mut names: IndexMap<String, &String> = IndexMap::new();
    for reference in refs {
        names.insert(
            phab_core::hash::digest_for_index(reference.as_bytes()),
            reference,
        );
    }
    let sql = format!(
        "SELECT c.refNameHash, c.refType, p.commitIdentifier, p.isClosed FROM {} c \
         JOIN {} p ON p.cursorID = c.id WHERE c.repositoryPHID = ? AND c.refNameHash IN ({})",
        database.table("repository", "repository_refcursor"),
        database.table("repository", "repository_refposition"),
        phab_db::placeholders(names.len())
    );
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql).bind(repository.phid.as_str()),
        names.keys().cloned(),
    );
    for row in query.fetch_all(database.pool()).await? {
        let hash = rows::text(&row, "refNameHash")?;
        if let Some(reference) = names.get(&hash) {
            add(
                &mut results,
                Key::from_text(reference),
                json!({
                    "type": rows::text(&row, "refType")?,
                    "identifier": rows::text(&row, "commitIdentifier")?,
                    "closed": rows::int(&row, "isClosed")? != 0,
                }),
            );
        }
    }
    if let Some(types) = types {
        filter_by_type(&mut results, types);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_hashes() {
        assert!(looks_like_hash("abcd"));
        assert!(looks_like_hash("abcd\n"));
        assert!(!looks_like_hash("abc"));
        assert!(!looks_like_hash("master"));
        assert!(is_full_hash("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_hash("0123456789ABCDEF0123456789abcdef01234567"));
    }

    #[test]
    fn filters_by_type_leaving_holes() {
        let mut resolved = Resolved::new();
        add(
            &mut resolved,
            Key::from_text("v1"),
            json!({"type": "branch"}),
        );
        add(&mut resolved, Key::from_text("v1"), json!({"type": "tag"}));
        add(
            &mut resolved,
            Key::from_text("x"),
            json!({"type": "branch"}),
        );
        filter_by_type(&mut resolved, &["tag".to_owned()]);
        assert_eq!(
            resolved_json(resolved),
            json!({"v1": {"1": {"type": "tag"}}})
        );
    }
}
