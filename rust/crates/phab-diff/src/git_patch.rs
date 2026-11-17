//! `ArcanistBundle::toGitPatch()` for text changes: what
//! `differential.getrawdiff` returns.
//!
//! Binary and image files (base85 `GIT binary patch` bodies) and
//! multicopies are not ported.

use std::fmt::{self, Write};

use serde_json::Value;
use thiserror::Error;

use crate::changeset::{ChangeType, Changeset, FileType};
use crate::hunk::Hunk;
use crate::lines;

/// What the patch can't be built from here.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PatchError {
    /// A binary or image file, whose body is a base85 binary patch.
    #[error("binary changes are not ported")]
    Binary,
    /// A multicopy, which arcanist decomposes into moves and copies.
    #[error("multicopies are not ported")]
    Multicopy,
    /// A file mode which is not a string.
    #[error("unusual file modes")]
    FileMode,
}

/// The number of context lines of the hunks.
const CONTEXT: usize = 5;

/// A hunk of the patch: its offsets, lengths and lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SmallHunk {
    old_offset: i64,
    old_len: i64,
    new_offset: i64,
    new_len: i64,
    corpus: String,
}

/// Appends formatted text; writing to a `String` can't fail.
fn push(out: &mut String, args: fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
}

fn first_byte(line: &str) -> Option<u8> {
    line.as_bytes().first().copied()
}

fn count(lines: usize) -> i64 {
    i64::try_from(lines).unwrap_or(i64::MAX)
}

/// `breakHunkIntoSmallHunks()`: the hunk cut where it has more than
/// `2 * CONTEXT` unchanged lines in a row, with `CONTEXT` lines of context.
fn small_hunks(hunk: &Hunk) -> Vec<SmallHunk> {
    let lines = lines::split_lines(&hunk.changes);
    let n = lines.len();
    let mut old_offset = hunk.old_offset;
    let mut new_offset = hunk.new_offset;
    let mut results = Vec::new();
    let mut ii = 0;
    while ii < n {
        // The next changed line, skipping context and "\" lines.
        let Some(first) = (ii..n).find(|&jj| matches!(first_byte(lines[jj]), Some(b'-' | b'+')))
        else {
            break;
        };
        let mut jj = first;
        let hunk_start = jj.saturating_sub(CONTEXT);
        let (mut old_lines, mut new_lines, mut hunk_adjust) = (0, 0, 0);
        let mut last_change = jj;
        let mut break_here = None;
        while jj < n {
            if first_byte(lines[jj]) == Some(b' ') {
                if jj - last_change > CONTEXT && break_here.is_none() {
                    break_here = Some(jj);
                }
                if jj - last_change > CONTEXT * 2 {
                    break;
                }
            } else {
                break_here = None;
                last_change = jj;
                match first_byte(lines[jj]) {
                    Some(b'\\') => hunk_adjust += 1,
                    Some(b'-') => old_lines += 1,
                    Some(b'+') => new_lines += 1,
                    _ => {}
                }
            }
            jj += 1;
        }
        if let Some(at) = break_here {
            jj = at;
        }
        let hunk_length = jj.min(n) - hunk_start;
        let count_length = count(hunk_length) - hunk_adjust;
        results.push(SmallHunk {
            old_offset: old_offset + count(hunk_start) - count(ii),
            new_offset: new_offset + count(hunk_start) - count(ii),
            old_len: count_length - new_lines,
            new_len: count_length - old_lines,
            corpus: lines[hunk_start..hunk_start + hunk_length].concat(),
        });
        old_offset += count(jj - ii) - new_lines;
        new_offset += count(jj - ii) - old_lines;
        ii = jj;
    }
    results
}

/// `buildHunkChanges()`.
fn hunk_changes(hunks: &[Hunk]) -> String {
    let mut result = String::new();
    for hunk in hunks.iter().flat_map(small_hunks) {
        let head = |offset: i64, length: i64| {
            if length == 1 {
                offset.to_string()
            } else {
                format!("{offset},{length}")
            }
        };
        push(
            &mut result,
            format_args!(
                "@@ -{} +{} @@\n",
                head(hunk.old_offset, hunk.old_len),
                head(hunk.new_offset, hunk.new_len)
            ),
        );
        result.push_str(&hunk.corpus);
        if !hunk.corpus.is_empty() && !hunk.corpus.ends_with(['\r', '\n']) {
            result.push('\n');
        }
    }
    result
}

/// `unix:filemode` of a property map, if set.
fn file_mode(properties: &Value) -> Result<Option<&str>, PatchError> {
    match properties.get("unix:filemode") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(mode)) => Ok(Some(mode)),
        Some(_) => Err(PatchError::FileMode),
    }
}

/// `encodeGitTargetPath()`: a path with spaces ends with a tab.
fn target(path: &str) -> String {
    if path.contains(' ') {
        format!("{path}\t")
    } else {
        path.to_owned()
    }
}

/// `toGitPatch()` of the changesets, in their order.
pub fn to_git_patch(changesets: &[Changeset]) -> Result<String, PatchError> {
    let mut result = String::new();
    for changeset in changesets {
        let change_type = changeset.change_type;
        if matches!(changeset.file_type, FileType::Binary | FileType::Image) {
            return Err(PatchError::Binary);
        }
        if change_type == ChangeType::Multicopy {
            return Err(PatchError::Multicopy);
        }
        // Git creates directories with their files; moves are applied
        // where the file moves to.
        if changeset.file_type == FileType::Directory || change_type == ChangeType::MoveAway {
            continue;
        }
        let old_mode = file_mode(&changeset.old_properties)?.unwrap_or("100644");
        let new_mode = file_mode(&changeset.new_properties)?.unwrap_or("100644");
        let body = hunk_changes(&changeset.hunks);
        if change_type == ChangeType::CopyAway && body.is_empty() && old_mode == new_mode {
            continue;
        }
        let old_path = changeset
            .old_file
            .as_deref()
            .filter(|path| !path.is_empty() && change_type != ChangeType::Add);
        let cur_path = Some(changeset.filename.as_str())
            .filter(|path| !path.is_empty() && change_type != ChangeType::Delete);
        let (old_index, old_target) = match old_path {
            None => (
                format!("a/{}", cur_path.unwrap_or_default()),
                "/dev/null".to_owned(),
            ),
            Some(path) => (format!("a/{path}"), format!("a/{path}")),
        };
        let (cur_index, cur_target) = match cur_path {
            None => (
                format!("b/{}", old_path.unwrap_or_default()),
                "/dev/null".to_owned(),
            ),
            Some(path) => (format!("b/{path}"), format!("b/{path}")),
        };
        push(
            &mut result,
            format_args!("diff --git {old_index} {cur_index}\n"),
        );
        if change_type == ChangeType::Add {
            push(&mut result, format_args!("new file mode {new_mode}\n"));
        }
        if matches!(
            change_type,
            ChangeType::CopyHere | ChangeType::MoveHere | ChangeType::CopyAway | ChangeType::Change
        ) && old_mode != new_mode
        {
            push(
                &mut result,
                format_args!("old mode {old_mode}\nnew mode {new_mode}\n"),
            );
        }
        match change_type {
            ChangeType::CopyHere => push(
                &mut result,
                format_args!(
                    "copy from {}\ncopy to {}\n",
                    old_path.unwrap_or_default(),
                    cur_path.unwrap_or_default()
                ),
            ),
            ChangeType::MoveHere => push(
                &mut result,
                format_args!(
                    "rename from {}\nrename to {}\n",
                    old_path.unwrap_or_default(),
                    cur_path.unwrap_or_default()
                ),
            ),
            ChangeType::Delete => {
                if let Some(mode) =
                    file_mode(&changeset.old_properties)?.filter(|mode| php_truthy(mode))
                {
                    push(&mut result, format_args!("deleted file mode {mode}\n"));
                }
            }
            _ => {}
        }
        if !body.is_empty() && body != "0" {
            push(
                &mut result,
                format_args!("--- {}\n+++ {}\n", target(&old_target), target(&cur_target)),
            );
            result.push_str(&body);
        }
    }
    result.push('\n');
    Ok(result)
}

/// PHP truthiness of a string.
fn php_truthy(text: &str) -> bool {
    !text.is_empty() && text != "0"
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn changeset(
        change_type: ChangeType,
        old: Option<&str>,
        new: &str,
        hunks: Vec<Hunk>,
    ) -> Changeset {
        Changeset {
            id: 1,
            phid: None,
            diff_id: 1,
            old_file: old.map(str::to_owned),
            filename: new.to_owned(),
            away_paths: Vec::new(),
            change_type,
            file_type: FileType::Text,
            metadata: json!({}),
            old_properties: json!([]),
            new_properties: json!([]),
            add_lines: 0,
            del_lines: 0,
            hunks,
        }
    }

    fn hunk(old_offset: i64, new_offset: i64, changes: &str) -> Hunk {
        Hunk {
            old_offset,
            old_len: 0,
            new_offset,
            new_len: 0,
            changes: changes.to_owned(),
        }
    }

    #[test]
    fn changes_like_arcanist() {
        let change = changeset(
            ChangeType::Change,
            Some("a.txt"),
            "a.txt",
            vec![hunk(1, 1, "-x\n+y\n")],
        );
        assert_eq!(
            to_git_patch(&[change]),
            Ok(
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n\n"
                    .to_owned()
            )
        );
    }

    #[test]
    fn adds_and_renames() {
        let mut add = changeset(
            ChangeType::Add,
            None,
            "new file.txt",
            vec![hunk(0, 1, "+a\n+b")],
        );
        add.new_properties = json!({"unix:filemode": "100755"});
        let rename = changeset(ChangeType::MoveHere, Some("old.txt"), "new.txt", Vec::new());
        let away = changeset(ChangeType::MoveAway, Some("old.txt"), "old.txt", Vec::new());
        assert_eq!(
            to_git_patch(&[add, rename, away]),
            Ok(
                "diff --git a/new file.txt b/new file.txt\nnew file mode 100755\n\
                --- /dev/null\n+++ b/new file.txt\t\n@@ -0,0 +1,2 @@\n+a\n+b\n\
                diff --git a/old.txt b/new.txt\nrename from old.txt\nrename to new.txt\n\n"
                    .to_owned()
            )
        );
    }

    #[test]
    fn splits_long_hunks() {
        let mut corpus = String::from("-a\n");
        for _ in 0..12 {
            corpus.push_str(" c\n");
        }
        corpus.push_str("+b\n");
        let hunks = small_hunks(&hunk(1, 1, &corpus));
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            (
                hunks[0].old_offset,
                hunks[0].old_len,
                hunks[0].new_offset,
                hunks[0].new_len
            ),
            (1, 6, 1, 5)
        );
        assert_eq!(
            (
                hunks[1].old_offset,
                hunks[1].old_len,
                hunks[1].new_offset,
                hunks[1].new_len
            ),
            (9, 5, 8, 6)
        );
    }

    #[test]
    fn binaries_are_left_to_php() {
        let mut binary = changeset(ChangeType::Add, None, "a.png", Vec::new());
        binary.file_type = FileType::Image;
        assert_eq!(to_git_patch(&[binary]), Err(PatchError::Binary));
    }
}
