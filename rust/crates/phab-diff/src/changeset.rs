//! `DifferentialChangeset`: one file of a diff, with its hunks.

use std::cmp::Ordering;

use serde_json::Value;

use crate::hunk::Hunk;

/// `DifferentialChangeType::TYPE_*`: what happened to the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeType {
    /// `TYPE_ADD` (1).
    Add,
    /// `TYPE_CHANGE` (2).
    Change,
    /// `TYPE_DELETE` (3).
    Delete,
    /// `TYPE_MOVE_AWAY` (4): the old location of a moved file.
    MoveAway,
    /// `TYPE_COPY_AWAY` (5): the source of a copy.
    CopyAway,
    /// `TYPE_MOVE_HERE` (6): the new location of a moved file.
    MoveHere,
    /// `TYPE_COPY_HERE` (7): the destination of a copy.
    CopyHere,
    /// `TYPE_MULTICOPY` (8): moved to several places.
    Multicopy,
    /// `TYPE_MESSAGE` (9): the commit message.
    Message,
    /// `TYPE_CHILD` (10): a change under a changed directory.
    Child,
    /// Any other stored value.
    Unknown(i64),
}

impl ChangeType {
    /// From the `changeType` column.
    pub fn from_value(value: i64) -> Self {
        match value {
            1 => Self::Add,
            2 => Self::Change,
            3 => Self::Delete,
            4 => Self::MoveAway,
            5 => Self::CopyAway,
            6 => Self::MoveHere,
            7 => Self::CopyHere,
            8 => Self::Multicopy,
            9 => Self::Message,
            10 => Self::Child,
            other => Self::Unknown(other),
        }
    }

    /// The `changeType` column.
    pub fn value(self) -> i64 {
        match self {
            Self::Add => 1,
            Self::Change => 2,
            Self::Delete => 3,
            Self::MoveAway => 4,
            Self::CopyAway => 5,
            Self::MoveHere => 6,
            Self::CopyHere => 7,
            Self::Multicopy => 8,
            Self::Message => 9,
            Self::Child => 10,
            Self::Unknown(other) => other,
        }
    }

    /// `DifferentialChangeType::isCreateChangeType()`.
    pub fn is_create(self) -> bool {
        matches!(self, Self::Add | Self::CopyHere | Self::MoveHere)
    }

    /// `DifferentialChangeType::isDeleteChangeType()`.
    pub fn is_delete(self) -> bool {
        matches!(self, Self::Delete | Self::MoveAway | Self::Multicopy)
    }

    /// `DifferentialChangeType::isOldLocationChangeType()`.
    pub fn is_old_location(self) -> bool {
        matches!(self, Self::MoveAway | Self::CopyAway | Self::Multicopy)
    }

    /// `DifferentialChangeType::isNewLocationChangeType()`.
    pub fn is_new_location(self) -> bool {
        matches!(self, Self::MoveHere | Self::CopyHere)
    }
}

/// `DifferentialChangeType::FILE_*`: what kind of file the path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// `FILE_TEXT` (1).
    Text,
    /// `FILE_IMAGE` (2).
    Image,
    /// `FILE_BINARY` (3).
    Binary,
    /// `FILE_DIRECTORY` (4).
    Directory,
    /// `FILE_SYMLINK` (5).
    Symlink,
    /// `FILE_DELETED` (6).
    Deleted,
    /// `FILE_NORMAL` (7).
    Normal,
    /// `FILE_SUBMODULE` (8).
    Submodule,
    /// Any other stored value.
    Unknown(i64),
}

impl FileType {
    /// From the `fileType` column.
    pub fn from_value(value: i64) -> Self {
        match value {
            1 => Self::Text,
            2 => Self::Image,
            3 => Self::Binary,
            4 => Self::Directory,
            5 => Self::Symlink,
            6 => Self::Deleted,
            7 => Self::Normal,
            8 => Self::Submodule,
            other => Self::Unknown(other),
        }
    }

    /// The `fileType` column.
    pub fn value(self) -> i64 {
        match self {
            Self::Text => 1,
            Self::Image => 2,
            Self::Binary => 3,
            Self::Directory => 4,
            Self::Symlink => 5,
            Self::Deleted => 6,
            Self::Normal => 7,
            Self::Submodule => 8,
            Self::Unknown(other) => other,
        }
    }
}

/// A `differential_changeset` row with its hunks.
#[derive(Debug, Clone, PartialEq)]
pub struct Changeset {
    /// The row ID.
    pub id: i64,
    /// The changeset PHID (added in 2021; older rows may have none).
    pub phid: Option<String>,
    /// The diff it belongs to.
    pub diff_id: i64,
    /// `oldFile`: the previous path of a moved or copied file.
    pub old_file: Option<String>,
    /// `filename`: the path.
    pub filename: String,
    /// `awayPaths`: where a file was moved or copied to.
    pub away_paths: Vec<String>,
    /// `changeType`.
    pub change_type: ChangeType,
    /// `fileType`.
    pub file_type: FileType,
    /// `metadata` (JSON).
    pub metadata: Value,
    /// `oldProperties` (JSON), such as the file mode.
    pub old_properties: Value,
    /// `newProperties` (JSON).
    pub new_properties: Value,
    /// `addLines`.
    pub add_lines: i64,
    /// `delLines`.
    pub del_lines: i64,
    /// The hunks, in ID order.
    pub hunks: Vec<Hunk>,
}

impl Changeset {
    /// `getDisplayFilename()`: directories get a trailing `/`.
    pub fn display_filename(&self) -> String {
        if self.file_type == FileType::Directory {
            format!("{}/", self.filename)
        } else {
            self.filename.clone()
        }
    }

    /// `getSortKey()`: the filename, with headers (`.h`, `.hpp`) before
    /// their implementations.
    pub fn sort_key(&self) -> String {
        self.filename.replace(".h", ".!h")
    }

    /// `hasOldState()`.
    pub fn has_old_state(&self) -> bool {
        !self.change_type.is_create()
    }

    /// `hasNewState()`.
    pub fn has_new_state(&self) -> bool {
        !self.change_type.is_delete()
    }

    /// `makeOldFile()`: the old side of every hunk, concatenated.
    pub fn make_old_file(&self) -> String {
        self.hunks.iter().map(Hunk::make_old_file).collect()
    }

    /// `makeNewFile()`.
    pub fn make_new_file(&self) -> String {
        self.hunks.iter().map(Hunk::make_new_file).collect()
    }

    /// `getAffectedLineCount()`.
    pub fn affected_line_count(&self) -> i64 {
        self.add_lines + self.del_lines
    }
}

/// `msort($changesets, 'getSortKey')` in `DifferentialRevisionViewController`:
/// a stable sort comparing the keys like PHP's `asort()`, where two numeric
/// strings compare as numbers.
pub fn sort_for_display(changesets: &mut [Changeset]) {
    changesets.sort_by(|a, b| php_compare(&a.sort_key(), &b.sort_key()));
}

/// PHP 8's `<=>` on two strings.
fn php_compare(a: &str, b: &str) -> Ordering {
    match (numeric_value(a), numeric_value(b)) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        _ => a.as_bytes().cmp(b.as_bytes()),
    }
}

/// The value of a PHP numeric string: optional whitespace, an optional
/// sign, digits with an optional fraction and exponent, optional trailing
/// whitespace.
fn numeric_value(text: &str) -> Option<f64> {
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C');
    let trimmed = text.trim_matches(is_space);
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        index += 1;
    }
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let integer = digits(index);
    index += integer;
    let mut fraction = 0;
    if bytes.get(index) == Some(&b'.') {
        fraction = digits(index + 1);
        index += 1 + fraction;
    }
    if integer == 0 && fraction == 0 {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let mut exponent = index + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let exponent_digits = digits(exponent);
        if exponent_digits > 0 {
            index = exponent + exponent_digits;
        }
    }
    if index != bytes.len() {
        return None;
    }
    trimmed.parse().ok()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn changeset(filename: &str) -> Changeset {
        Changeset {
            id: 1,
            phid: None,
            diff_id: 1,
            old_file: None,
            filename: filename.to_owned(),
            away_paths: Vec::new(),
            change_type: ChangeType::Change,
            file_type: FileType::Text,
            metadata: Value::Null,
            old_properties: Value::Null,
            new_properties: Value::Null,
            add_lines: 0,
            del_lines: 0,
            hunks: Vec::new(),
        }
    }

    #[test]
    fn headers_sort_first() {
        let mut changesets: Vec<Changeset> = ["b.cpp", "b.h", "a.cpp", "10", "9"]
            .into_iter()
            .map(changeset)
            .collect();
        sort_for_display(&mut changesets);
        let names: Vec<&str> = changesets.iter().map(|c| c.filename.as_str()).collect();
        assert_eq!(names, ["9", "10", "a.cpp", "b.h", "b.cpp"]);
    }

    #[rstest]
    #[case("10", Some(10.0))]
    #[case(" 1.5 ", Some(1.5))]
    #[case("-2e3", Some(-2000.0))]
    #[case(".5", Some(0.5))]
    #[case("1e", None)]
    #[case("1.2.3", None)]
    #[case("inf", None)]
    #[case("", None)]
    #[case("0x1A", None)]
    fn numeric_strings(#[case] text: &str, #[case] expected: Option<f64>) {
        assert_eq!(numeric_value(text), expected);
    }

    #[test]
    fn directories_have_a_slash() {
        let mut directory = changeset("src");
        directory.file_type = FileType::Directory;
        assert_eq!(directory.display_filename(), "src/");
        assert_eq!(changeset("a.c").display_filename(), "a.c");
    }
}
