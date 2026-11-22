//! `diffusion.tagsquery` (`DiffusionTagsQueryConduitAPIMethod`): the tags
//! of a Git repository, from `git for-each-ref`, with their messages from
//! `git cat-file tag`.
//!
//! Ported for Git: every parameter. Left to PHP: values of unusual types.

use std::collections::HashSet;

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use phab_core::php as core_php;
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::git::WorkingCopy;
use crate::php::{self, Key};
use crate::query::QueryMethod;
use crate::refs::{self, GitRef, TAG};

/// `loadTagNamesForCommit()`: the tags containing `commit`.
async fn tags_containing(copy: &WorkingCopy, commit: String) -> Result<HashSet<Key>, MethodError> {
    let output = copy
        .exec(
            vec![
                "tag".to_owned(),
                "-l".to_owned(),
                "--contains".to_owned(),
                commit,
            ],
            None,
        )
        .await?;
    if !output.success() {
        // Git exits with an error code if the commit is bogus.
        return Ok(HashSet::new());
    }
    let stdout = php::text(output.stdout)?;
    let stdout = stdout.trim_end_matches('\n');
    if stdout.is_empty() {
        return Ok(HashSet::new());
    }
    Ok(stdout.split('\n').map(Key::from_text).collect())
}

/// The message of an annotated tag (`loadMessagesForTags()`).
async fn message(copy: &WorkingCopy, name: &str) -> Result<Value, MethodError> {
    let output = copy
        .exec(
            vec!["cat-file".to_owned(), "tag".to_owned(), name.to_owned()],
            None,
        )
        .await?;
    if !output.success() {
        // Lightweight tags are not "tag" objects.
        return Ok(Value::Null);
    }
    let stdout = php::text(output.stdout)?;
    Ok(match stdout.split_once("\n\n") {
        Some((_, message)) => json!(message),
        None => Value::Null,
    })
}

/// `DiffusionRepositoryTag::toDictionary()`.
fn tag_dictionary(tag: &GitRef) -> Value {
    json!({
        "author": tag.fields.get("author"),
        "epoch": tag.fields.get("epoch"),
        "commitIdentifier": tag.commit,
        "name": tag.short_name,
        "description": tag.fields.get("subject"),
        "type": format!("git/{}", tag.field("objecttype").unwrap_or_default()),
    })
}

/// `diffusion.tagsquery`.
pub struct TagsQuery;

impl QueryMethod for TagsQuery {
    fn name(&self) -> &'static str {
        "diffusion.tagsquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "names",
            "commit",
            "needMessages",
            "offset",
            "limit",
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
            let copy = &drequest.copy;
            let commit_filter = match drequest.symbolic_commit() {
                Some(commit) if core_php::is_truthy(commit) => {
                    Some(tags_containing(copy, php::shell_string(Some(commit))?).await?)
                }
                _ => None,
            };
            let name_filter: Option<HashSet<Key>> = match request.value("names") {
                None | Some(Value::Null) => None,
                Some(Value::Array(names)) => Some(
                    names
                        .iter()
                        .map(|name| match name {
                            Value::String(name) => Some(Key::from_text(name)),
                            Value::Number(number) => number.as_i64().map(Key::Int),
                            _ => None,
                        })
                        .collect::<Option<_>>()
                        .ok_or(MethodError::Unsupported("tag names which are not strings"))?,
                ),
                Some(_) => return Err(MethodError::Unsupported("tag names which are not a list")),
            };

            // mpull($all_tags, null, 'getName'): the last tag of a name, in
            // the place of the first.
            let mut all_tags: indexmap::IndexMap<Key, GitRef> = indexmap::IndexMap::new();
            for tag in refs::git_refs(copy, &[TAG]).await? {
                all_tags.insert(Key::from_text(&tag.short_name), tag);
            }
            if let Some(names) = &name_filter {
                all_tags.retain(|key, _| names.contains(key));
            }
            if let Some(commit_filter) = &commit_filter {
                all_tags.retain(|key, _| commit_filter.contains(key));
            }
            let mut tags: Vec<GitRef> = all_tags.into_values().collect();
            let offset = request.value("offset");
            if offset.is_some_and(core_php::is_truthy) {
                let range = php::slice(tags.len(), php::int_param(offset, 0)?, None);
                tags = tags.drain(range).collect();
            }
            let limit = request.value("limit");
            if limit.is_some_and(core_php::is_truthy) {
                let range = php::slice(tags.len(), 0, Some(php::int_param(limit, 0)?));
                tags = tags.drain(range).collect();
            }
            let need_messages = request
                .value("needMessages")
                .is_some_and(core_php::is_truthy);
            let mut results = Vec::new();
            for tag in &tags {
                let mut dictionary = tag_dictionary(tag);
                if need_messages {
                    dictionary["message"] = message(copy, &tag.short_name).await?;
                }
                results.push(dictionary);
            }
            Ok(Value::Array(results))
        })
    }
}
