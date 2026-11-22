//! The changesets and hunks of a new diff, as
//! `DifferentialDiff::newFromRawChanges()` builds them from arcanist's
//! changes, then `DifferentialChangesetEngine::rebuildChangesets()` adds
//! their generated code attributes, effect hashes and copied lines.

use std::collections::HashMap;

use pcre2::bytes::Regex;
use serde_json::{Map, Value, json};

use crate::arcanist::{RawChange, change_type};
use crate::hunk::Hunk;
use crate::lines;

/// A changeset to insert, with its hunks.
#[derive(Debug, Clone, PartialEq)]
pub struct NewChangeset {
    /// `oldFile`.
    pub old_file: Option<String>,
    /// `filename`.
    pub filename: String,
    /// `changeType`.
    pub change_type: i64,
    /// `fileType`.
    pub file_type: i64,
    /// `metadata`, as PHP holds it (`None` for `null`).
    pub metadata: Option<Map<String, Value>>,
    /// `oldProperties`.
    pub old_properties: Value,
    /// `newProperties`.
    pub new_properties: Value,
    /// `awayPaths`.
    pub away_paths: Value,
    /// `addLines`.
    pub add_lines: i64,
    /// `delLines`.
    pub del_lines: i64,
    /// The hunks; their `changes` are the corpus.
    pub hunks: Vec<Hunk>,
}

/// What can't be built the way PHP builds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported(pub &'static str);

/// `DifferentialDiff::buildChangesetsFromRawChanges()` up to the engine:
/// the changesets (messages are dropped) and the diff's `lineCount`.
pub fn changesets(changes: &[RawChange]) -> Result<(Vec<NewChangeset>, i64), Unsupported> {
    let mut lines = 0;
    let mut changesets = Vec::new();
    for change in changes {
        if change.change_type == change_type::MESSAGE {
            continue;
        }
        let mut add_lines = 0;
        let mut del_lines = 0;
        let mut first_line: Option<i64> = None;
        let mut hunks = Vec::with_capacity(change.hunks.len());
        for hunk in &change.hunks {
            hunks.push(Hunk {
                old_offset: hunk.old_offset,
                old_len: hunk.old_length,
                new_offset: hunk.new_offset,
                new_len: hunk.new_length,
                changes: hunk.corpus.clone(),
            });
            add_lines += hunk.add_lines;
            del_lines += hunk.del_lines;
            if let Some(first) = first_added_line(hunk.new_offset, &hunk.corpus) {
                first_line = Some(first_line.map_or(first, |line| line.min(first)));
            }
        }
        lines += add_lines + del_lines;
        let mut metadata = change.metadata.clone();
        if let Some(first) = first_line {
            metadata
                .get_or_insert_with(Map::new)
                .insert("line:first".to_owned(), json!(first));
        }
        let filename = change
            .current_path
            .clone()
            .ok_or(Unsupported("changes without a current path"))?;
        changesets.push(NewChangeset {
            old_file: change.old_path.clone(),
            filename,
            change_type: change.change_type,
            file_type: change.file_type,
            metadata,
            old_properties: change.old_properties.clone(),
            new_properties: change.new_properties.clone(),
            away_paths: change.away_paths.clone(),
            add_lines,
            del_lines,
            hunks,
        });
    }
    Ok((changesets, lines))
}

/// The first key of `ArcanistDiffHunk::getChangedLines('new')`.
fn first_added_line(new_offset: i64, corpus: &str) -> Option<i64> {
    let mut line = new_offset;
    for text in corpus.split('\n') {
        match text.as_bytes().first() {
            Some(b'-') => {}
            Some(b'+') => return Some(line),
            _ => line += 1,
        }
    }
    None
}

impl NewChangeset {
    /// `getNewFileObjectPHID()`: `metadata['new:binary-phid']`.
    pub fn new_file_phid(&self) -> Result<Option<&str>, Unsupported> {
        match self
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("new:binary-phid"))
        {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(phid)) => Ok(Some(phid)),
            Some(_) => Err(Unsupported("binary PHIDs which are not strings")),
        }
    }

    /// `makeNewFile()`.
    fn make_new_file(&self) -> String {
        self.hunks.iter().map(Hunk::make_new_file).collect()
    }

    /// `setChangesetMetadata()`.
    fn set_metadata(&mut self, key: &str, value: Value) {
        self.metadata
            .get_or_insert_with(Map::new)
            .insert(key.to_owned(), value);
    }

    /// `setInternalChangesetAttribute()`.
    fn set_attribute(&mut self, metadata_key: &str, key: &str) -> Result<(), Unsupported> {
        let mut attributes = match self
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(metadata_key))
        {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value.clone()))
                .collect(),
            Some(_) => {
                return Err(Unsupported("changeset attributes which are not arrays"));
            }
        };
        attributes.insert(key.to_owned(), Value::Bool(true));
        self.set_metadata(metadata_key, Value::Object(attributes));
        Ok(())
    }
}

/// The new file of a binary changeset, as the engine reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileObject {
    /// `contentHash`.
    pub content_hash: Option<String>,
}

/// `// Code generated .* DO NOT EDIT.` on a line of its own
/// (`(^// Code generated .* DO NOT EDIT\.$)m`).
fn has_go_generated_marker(data: &str) -> bool {
    const PREFIX: &str = "// Code generated ";
    const SUFFIX: &str = " DO NOT EDIT.";
    data.split('\n').any(|line| {
        line.len() >= PREFIX.len() + SUFFIX.len()
            && line.starts_with(PREFIX)
            && line.ends_with(SUFFIX)
    })
}

/// `DifferentialChangesetEngine::rebuildChangesets()`. `generated_paths`
/// are the compiled `differential.generated-paths`; `files` the new files
/// of binary changesets by PHID, as the omnipotent viewer loads them
/// (missing ones take their changeset out of the engine's work, as PHP's
/// `loadChangesetFiles()` does).
pub fn rebuild<S: std::hash::BuildHasher>(
    changesets: &mut [NewChangeset],
    generated_paths: &[Regex],
    files: &HashMap<String, FileObject, S>,
) -> Result<(), Unsupported> {
    let mut kept = Vec::new();
    for (index, changeset) in changesets.iter().enumerate() {
        match changeset.new_file_phid()? {
            Some(phid) if !files.contains_key(phid) => {}
            _ => kept.push(index),
        }
    }
    for &index in &kept {
        let changeset = &mut changesets[index];
        let trusted = generated_paths.iter().any(|regex| {
            regex
                .is_match(changeset.filename.as_bytes())
                .unwrap_or(false)
        });
        if trusted {
            changeset.set_attribute("attributes.trusted", "generated")?;
        }
        if !changeset.hunks.is_empty() {
            let new_data = changeset.make_new_file();
            if new_data.contains("@generated") || has_go_generated_marker(&new_data) {
                changeset.set_attribute("attributes.untrusted", "generated")?;
            }
        }
        let hash = if changeset.hunks.is_empty() {
            changeset
                .new_file_phid()?
                .and_then(|phid| files.get(phid))
                .and_then(|file| file.content_hash.as_ref())
                .map(|hash| {
                    phab_core::hash::digest_for_index(format!("file-hash:{hash}").as_bytes())
                })
        } else {
            Some(phab_core::hash::digest_for_index(
                changeset.make_new_file().as_bytes(),
            ))
        };
        if let Some(hash) = hash {
            changeset.set_metadata("hash.effect", Value::String(hash));
        }
    }
    detect_copied_code(changesets, &kept)
}

/// A line of `getStructuredOldFile()` or `getStructuredNewFile()`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StructuredLine {
    kind: u8,
    text: String,
}

/// `DifferentialHunk::getStructuredFile()`, in line order.
fn structured_file(hunk: &Hunk, kind: u8) -> Result<Vec<(i64, StructuredLine)>, Unsupported> {
    let mut number = if kind == b'+' {
        hunk.new_offset
    } else {
        hunk.old_offset
    };
    let mut file: Vec<(i64, StructuredLine)> = Vec::new();
    let mut keep = true;
    for line in lines::split_lines(&hunk.changes) {
        // `empty($line[0])`: no character, or "0".
        let Some(&first) = line.as_bytes().first() else {
            continue;
        };
        if first == b'0' {
            continue;
        }
        let structured = StructuredLine {
            kind: first,
            text: line[1..].to_owned(),
        };
        if first == b' ' || first == kind {
            file.push((number, structured));
            number += 1;
            keep = true;
        } else if first == b'\\' {
            if keep {
                let Some((_, previous)) = file.last_mut() else {
                    return Err(Unsupported("hunks starting with a no-newline marker"));
                };
                let trimmed = previous.text.trim_end_matches(['\r', '\n']).len();
                previous.text.truncate(trimmed);
                file.push((number, structured));
                number += 1;
                keep = false;
            }
        } else {
            keep = false;
        }
    }
    Ok(file)
}

/// PHP's `trim()`.
fn php_trim(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// An entry of `copy:lines`; `None` is the `array()` of a short block.
type Copy = Option<(String, i64, Option<u8>)>;

/// The old sides of the changesets, as `detectCopiedCode()` indexes them:
/// the long lines (`$map`), and the text and type of every line by file.
#[derive(Default)]
struct OldLines {
    map: HashMap<String, Vec<(String, i64)>>,
    files: HashMap<String, HashMap<i64, String>>,
    types: HashMap<String, HashMap<i64, u8>>,
}

/// Where a block of added lines comes from.
struct Source<'a> {
    length: i64,
    /// How many lines after the added one match too.
    offset: i64,
    file: &'a str,
    line: i64,
}

impl OldLines {
    /// Lines shorter than this are not looked for.
    const MIN_WIDTH: usize = 30;

    fn new(changesets: &[NewChangeset], kept: &[usize]) -> Result<Self, Unsupported> {
        let mut old = Self::default();
        for &index in kept {
            let changeset = &changesets[index];
            let file = &changeset.filename;
            for hunk in &changeset.hunks {
                for (line, info) in structured_file(hunk, b'-')? {
                    if info.kind == b'\\' {
                        continue;
                    }
                    old.types
                        .entry(file.clone())
                        .or_default()
                        .insert(line, info.kind);
                    let text = php_trim(&info.text).to_owned();
                    if text.len() >= Self::MIN_WIDTH {
                        old.map
                            .entry(text.clone())
                            .or_default()
                            .push((file.clone(), line));
                    }
                    old.files
                        .entry(file.clone())
                        .or_default()
                        .insert(line, text);
                }
            }
        }
        Ok(old)
    }

    fn kind(&self, file: &str, line: i64) -> Option<u8> {
        self.types.get(file)?.get(&line).copied()
    }

    /// The longest block of old lines around a match of the added `line`,
    /// moves first; `None` when the line is too short or too common.
    fn best_source(
        &self,
        line: i64,
        added: &HashMap<i64, String>,
        copies: &Copies,
    ) -> Option<Source<'_>> {
        let candidates = self.map.get(&added[&line])?;
        if candidates.is_empty() || candidates.len() > 16 {
            return None;
        }
        let mut best: Option<Source<'_>> = None;
        for (file, orig_line) in candidates {
            let file_lines = self.files.get(file);
            let mut length = 1;
            let mut offset = 0;
            for direction in [-1, 1] {
                offset = direction;
                while !copies.contains(line + offset) {
                    let Some(new) = added.get(&(line + offset)) else {
                        break;
                    };
                    let old = file_lines.and_then(|lines| lines.get(&(orig_line + offset)));
                    if old != Some(new) {
                        break;
                    }
                    length += 1;
                    offset += direction;
                }
            }
            let best_length = best.as_ref().map_or(0, |best| best.length);
            if length < best_length
                || length == best_length && self.kind(file, *orig_line) != Some(b'-')
            {
                continue;
            }
            best = Some(Source {
                length,
                offset: offset - 1,
                file,
                line: *orig_line,
            });
        }
        best
    }
}

/// `$copies` of a changeset, in insertion order.
#[derive(Default)]
struct Copies {
    entries: Vec<(i64, Copy)>,
    positions: HashMap<i64, usize>,
}

impl Copies {
    fn contains(&self, line: i64) -> bool {
        self.positions.contains_key(&line)
    }

    fn set(&mut self, line: i64, entry: Copy) {
        if let Some(&position) = self.positions.get(&line) {
            self.entries[position].1 = entry;
        } else {
            self.positions.insert(line, self.entries.len());
            self.entries.push((line, entry));
        }
    }

    /// `array_filter($copies)`.
    fn into_metadata(self) -> Map<String, Value> {
        self.entries
            .into_iter()
            .filter_map(|(line, entry)| {
                let (file, source, kind) = entry?;
                let kind = kind.map(|kind| Value::String(char::from(kind).to_string()));
                Some((line.to_string(), json!([file, source, kind])))
            })
            .collect()
    }
}

/// `detectCopiedCode()` over the changesets the engine kept.
fn detect_copied_code(changesets: &mut [NewChangeset], kept: &[usize]) -> Result<(), Unsupported> {
    const MAX_SIZE: i64 = 65535;
    const MIN_LINES: i64 = 3;
    let total: i64 = kept
        .iter()
        .map(|&index| changesets[index].add_lines + changesets[index].del_lines)
        .sum();
    if total > MAX_SIZE {
        return Ok(());
    }
    let old = OldLines::new(changesets, kept)?;
    for &index in kept {
        let mut copies = Copies::default();
        let own = &changesets[index].filename;
        for hunk in &changesets[index].hunks {
            let structured = structured_file(hunk, b'+')?;
            let added: HashMap<i64, String> = structured
                .iter()
                .map(|(line, info)| (*line, php_trim(&info.text).to_owned()))
                .collect();
            let mut skip_lines = 0;
            // Line numbers of a structured file only increase.
            for (line, info) in &structured {
                if skip_lines > 0 {
                    skip_lines -= 1;
                    continue;
                }
                if info.kind != b'+' {
                    continue;
                }
                let Some(source) = old.best_source(*line, &added, &copies) else {
                    continue;
                };
                // `==`: numeric names may be equal without being the same.
                let label = if source.file == own {
                    String::new()
                } else if is_php_numeric(source.file) && is_php_numeric(own) {
                    return Err(Unsupported("file names which are numbers"));
                } else {
                    source.file.to_owned()
                };
                for i in (0..source.length).rev() {
                    let from = source.line + source.offset - i;
                    let entry = (source.length >= MIN_LINES)
                        .then(|| (label.clone(), from, old.kind(source.file, from)));
                    copies.set(line + source.offset - i, entry);
                }
                skip_lines = source.offset;
            }
        }
        let copies = copies.into_metadata();
        if !copies.is_empty() {
            changesets[index].set_metadata("copy:lines", Value::Object(copies));
        }
    }
    Ok(())
}

/// Whether `==` could compare `text` as a number (a PHP numeric string),
/// approximately: then two different names may be equal.
fn is_php_numeric(text: &str) -> bool {
    let trimmed = text.trim_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
    !trimmed.is_empty()
        && trimmed.bytes().any(|byte| byte.is_ascii_digit())
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arcanist::{RawHunk, parse_diff};

    fn built(diff: &str) -> Vec<NewChangeset> {
        let (mut changesets, _) = changesets(&parse_diff(diff).unwrap()).unwrap();
        rebuild(&mut changesets, &[], &HashMap::new()).unwrap();
        changesets
    }

    #[test]
    fn adds_the_first_line_and_the_hash() {
        let changesets =
            built("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -3,3 +3,3 @@\n a\n-b\n+c\n d\n");
        let metadata = changesets[0].metadata.as_ref().unwrap();
        assert_eq!(metadata["line:first"], json!(4));
        assert_eq!(
            metadata["hash.effect"],
            json!(phab_core::hash::digest_for_index(b"a\nc\nd\n"))
        );
        assert_eq!((changesets[0].add_lines, changesets[0].del_lines), (1, 1));
    }

    #[test]
    fn marks_generated_code() {
        let changesets = built(
            "diff --git a/g.go b/g.go\nnew file mode 100644\n--- /dev/null\n+++ b/g.go\n\
             @@ -0,0 +1,2 @@\n+// Code generated by x. DO NOT EDIT.\n+package g\n",
        );
        let metadata = changesets[0].metadata.as_ref().unwrap();
        assert_eq!(metadata["attributes.untrusted"], json!({"generated": true}));
    }

    #[test]
    fn detects_moved_code() {
        let block = [
            "first line of a moved block of code;",
            "second line of a moved block of code;",
            "third line of a moved block of code;",
        ];
        let removed: String = block.iter().flat_map(|line| ["-", line, "\n"]).collect();
        let added: String = block.iter().flat_map(|line| ["+", line, "\n"]).collect();
        let diff = format!(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,3 +0,0 @@\n{removed}\
             diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1,0 +1,3 @@\n{added}"
        );
        let changesets = built(&diff);
        assert_eq!(
            changesets[1].metadata.as_ref().unwrap()["copy:lines"],
            json!({"1": ["a", 1, "-"], "2": ["a", 2, "-"], "3": ["a", 3, "-"]})
        );
    }

    #[test]
    fn drops_messages_and_keeps_client_metadata() {
        let message = RawChange {
            change_type: change_type::MESSAGE,
            ..RawChange::default()
        };
        let change = RawChange {
            current_path: Some("x".to_owned()),
            metadata: None,
            hunks: vec![RawHunk {
                old_offset: 1,
                old_length: 1,
                new_offset: 1,
                new_length: 1,
                add_lines: 1,
                del_lines: 1,
                corpus: "-a\n+b\n".to_owned(),
            }],
            ..RawChange::default()
        };
        let (changesets, lines) = changesets(&[message, change]).unwrap();
        assert_eq!(changesets.len(), 1);
        assert_eq!(lines, 2);
        assert_eq!(
            changesets[0].metadata,
            Some(Map::from_iter([("line:first".to_owned(), json!(1))]))
        );
    }
}
