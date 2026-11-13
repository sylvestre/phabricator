//! `DifferentialChangesetParser::realignDiff()`: when a changeset has the
//! whole files (one hunk starting at line 1), PHP rediffs them with
//! whitespace removed and takes the line types from that diff, so lines
//! that only changed whitespace are aligned with each other.
//!
//! The rediff runs the system `diff` like `PhabricatorDifferenceEngine`.
//! Implementations align some changes differently, so for parity the
//! command must be the one PHP runs: the PHP image is Alpine, where `diff`
//! comes from `BusyBox`.

use std::io::Write;
use std::process::Command;

use thiserror::Error;

use crate::changeset::Changeset;
use crate::parser::{HunkParser, ParseError};
use crate::unified::{self, UnifiedError};

/// The command which compares two files, `diff` by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCommand {
    program: String,
    arguments: Vec<String>,
}

impl Default for DiffCommand {
    fn default() -> Self {
        Self::new("diff", &[])
    }
}

impl DiffCommand {
    /// A command such as `busybox` with arguments `["diff"]`.
    pub fn new(program: &str, arguments: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            arguments: arguments
                .iter()
                .map(|&argument| argument.to_owned())
                .collect(),
        }
    }

    /// A command from a space-separated string such as `busybox diff`;
    /// `None` for an empty string.
    pub fn parse(command: &str) -> Option<Self> {
        let mut words = command.split_whitespace();
        let program = words.next()?;
        Some(Self {
            program: program.to_owned(),
            arguments: words.map(str::to_owned).collect(),
        })
    }
}

/// Why the realignment failed. PHP throws in these cases.
#[derive(Debug, Error)]
pub enum RealignError {
    /// The temporary files or the command failed.
    #[error("running diff failed: {0}")]
    Io(#[from] std::io::Error),
    /// `diff` exited with an error (status 2 or a signal).
    #[error("diff failed with {0}")]
    Diff(std::process::ExitStatus),
    /// The output of `diff` could not be read.
    #[error(transparent)]
    Unified(#[from] UnifiedError),
    /// The rediffed hunks could not be parsed.
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// `PhabricatorDifferenceEngine::normalizeFile()`: spaces and tabs removed.
fn normalize(corpus: &str) -> String {
    corpus.chars().filter(|&c| c != ' ' && c != '\t').collect()
}

const LABEL: &str = "/dev/universe 9999-99-99";

/// `PhabricatorDifferenceEngine::generateRawDiffFromFileContent()` with
/// normalization: a full-context diff of the normalized files.
pub fn normalized_raw_diff(
    old: &str,
    new: &str,
    command: &DiffCommand,
) -> Result<String, RealignError> {
    let (old, new) = (normalize(old), normalize(new));
    let mut old_file = tempfile::NamedTempFile::new()?;
    let mut new_file = tempfile::NamedTempFile::new()?;
    old_file.write_all(old.as_bytes())?;
    new_file.write_all(new.as_bytes())?;
    old_file.flush()?;
    new_file.flush()?;

    let output = Command::new(&command.program)
        .args(&command.arguments)
        .args(["-U65535", "-L", LABEL, "-L", LABEL])
        .arg(old_file.path())
        .arg(new_file.path())
        .output()?;
    match output.status.code() {
        // The files are the same: a changeless diff of the whole file.
        Some(0) => {
            let lines: Vec<String> = old.split('\n').map(|line| format!(" {line}")).collect();
            let length = lines.len();
            Ok(format!(
                "--- {LABEL}\n+++ {LABEL}\n@@ -1,{length} +1,{length} @@\n{}\n",
                lines.join("\n")
            ))
        }
        Some(1) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        _ => Err(RealignError::Diff(output.status)),
    }
}

/// `realignDiff()`: replaces the line types of `parser`, built from
/// `changeset`'s hunks, with those of the whitespace-insensitive diff.
/// Changesets without the whole files, or with identical sides, are left
/// alone. Runs a process: call it from a blocking task.
pub fn realign(
    changeset: &Changeset,
    parser: &mut HunkParser,
    command: &DiffCommand,
) -> Result<(), RealignError> {
    let [hunk] = changeset.hunks.as_slice() else {
        return Ok(());
    };
    if hunk.old_offset != 1 || hunk.new_offset != 1 {
        return Ok(());
    }
    let old = changeset.make_old_file();
    let new = changeset.make_new_file();
    if old == new {
        // e.g. a file copied onto itself (T271): the rediff is nonsense.
        return Ok(());
    }
    let diff = normalized_raw_diff(&old, &new, command)?;
    let normalized = HunkParser::parse(&unified::parse_hunks(&diff)?)?;
    parser.normalized = true;
    parser.set_old_line_type_map(&normalized.old_line_type_map());
    parser.set_new_line_type_map(&normalized.new_line_type_map());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_is_removed() {
        assert_eq!(normalize(" a\tb c\r\n"), "abc\r\n");
    }

    #[test]
    fn commands_parse() {
        assert_eq!(
            DiffCommand::parse("busybox diff"),
            Some(DiffCommand::new("busybox", &["diff"]))
        );
        assert_eq!(DiffCommand::parse("  "), None);
    }

    #[test]
    fn identical_files_give_a_changeless_diff() {
        let diff = normalized_raw_diff("a\n b\n", "a\nb\n", &DiffCommand::default()).unwrap();
        assert_eq!(
            diff,
            format!("--- {LABEL}\n+++ {LABEL}\n@@ -1,3 +1,3 @@\n a\n b\n \n")
        );
    }
}
