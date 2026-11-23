//! Arcanist's changes (`ArcanistDiffChange`, `ArcanistDiffHunk`) and the
//! Git part of its raw diff parser (`ArcanistDiffParser::parseDiff()`),
//! which `differential.createrawdiff` runs.
//!
//! Only Git diffs (`diff --git` headers) are parsed. Anything else the
//! parser accepts (Subversion, Mercurial, `git show` commit messages,
//! `git format-patch` mails, plain unified diffs, colorized diffs), and any
//! diff it rejects (with an error message quoting the input), is
//! [`ParseError::Unsupported`].

use std::sync::OnceLock;

use pcre2::bytes::Regex;
use serde_json::{Map, Value};

use crate::lines;

/// `ArcanistDiffChangeType::TYPE_*`.
pub mod change_type {
    /// `TYPE_ADD`.
    pub const ADD: i64 = 1;
    /// `TYPE_CHANGE`.
    pub const CHANGE: i64 = 2;
    /// `TYPE_DELETE`.
    pub const DELETE: i64 = 3;
    /// `TYPE_MOVE_AWAY`.
    pub const MOVE_AWAY: i64 = 4;
    /// `TYPE_COPY_AWAY`.
    pub const COPY_AWAY: i64 = 5;
    /// `TYPE_MOVE_HERE`.
    pub const MOVE_HERE: i64 = 6;
    /// `TYPE_COPY_HERE`.
    pub const COPY_HERE: i64 = 7;
    /// `TYPE_MULTICOPY`.
    pub const MULTICOPY: i64 = 8;
    /// `TYPE_MESSAGE`.
    pub const MESSAGE: i64 = 9;
}

/// `ArcanistDiffChangeType::FILE_TEXT`.
pub const FILE_TEXT: i64 = 1;
/// `ArcanistDiffChangeType::FILE_BINARY`.
pub const FILE_BINARY: i64 = 3;

/// An `ArcanistDiffHunk`, with the values `DifferentialDiff` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawHunk {
    /// `oldOffset`.
    pub old_offset: i64,
    /// `oldLength`.
    pub old_length: i64,
    /// `newOffset`.
    pub new_offset: i64,
    /// `newLength`.
    pub new_length: i64,
    /// `addLines`.
    pub add_lines: i64,
    /// `delLines`.
    pub del_lines: i64,
    /// `corpus`.
    pub corpus: String,
    /// `isMissingOldNewline`.
    pub is_missing_old_newline: bool,
    /// `isMissingNewNewline`.
    pub is_missing_new_newline: bool,
    /// Whether the header gave the old and the new lengths: the parser
    /// keeps them as the strings it matched, and `1` (an integer) when
    /// they are missing.
    pub lengths_given: (bool, bool),
}

impl RawHunk {
    /// `ArcanistDiffHunk::toDictionary()` of a parsed hunk, whose offsets
    /// (and lengths, when the header has them) are the strings the parser
    /// matched.
    pub fn to_dictionary(&self) -> Value {
        let length = |value: i64, given: bool| {
            if given {
                Value::String(value.to_string())
            } else {
                Value::from(value)
            }
        };
        let mut dictionary = Map::new();
        for (key, value) in [
            ("oldOffset", Value::String(self.old_offset.to_string())),
            ("newOffset", Value::String(self.new_offset.to_string())),
            ("oldLength", length(self.old_length, self.lengths_given.0)),
            ("newLength", length(self.new_length, self.lengths_given.1)),
            ("addLines", Value::from(self.add_lines)),
            ("delLines", Value::from(self.del_lines)),
            (
                "isMissingOldNewline",
                Value::Bool(self.is_missing_old_newline),
            ),
            (
                "isMissingNewNewline",
                Value::Bool(self.is_missing_new_newline),
            ),
            ("corpus", Value::String(self.corpus.clone())),
        ] {
            dictionary.insert(key.to_owned(), value);
        }
        Value::Object(dictionary)
    }
}

/// An `ArcanistDiffChange`. The array values keep PHP's arrays: maps, in
/// order, which are lists when keyed `0..n` (see [`phab_core::php`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RawChange {
    /// `metadata`; `None` for a `null` from `newFromDictionary()`.
    pub metadata: Option<Map<String, Value>>,
    /// `oldPath`.
    pub old_path: Option<String>,
    /// `currentPath`.
    pub current_path: Option<String>,
    /// `awayPaths`.
    pub away_paths: Value,
    /// `oldProperties`.
    pub old_properties: Value,
    /// `newProperties`.
    pub new_properties: Value,
    /// `type`.
    pub change_type: i64,
    /// `fileType`.
    pub file_type: i64,
    /// `hunks`.
    pub hunks: Vec<RawHunk>,
}

impl RawChange {
    /// `ArcanistDiffChange::toDictionary()`, with PHP's arrays as
    /// `json_encode()` writes them. Git diffs have no commit hash.
    pub fn to_dictionary(&self) -> Value {
        let metadata = self.metadata.clone().map_or(Value::Null, Value::Object);
        let optional = |path: &Option<String>| path.clone().map_or(Value::Null, Value::String);
        let mut dictionary = Map::new();
        for (key, value) in [
            ("metadata", phab_core::php::round_trip(&metadata)),
            ("oldPath", optional(&self.old_path)),
            ("currentPath", optional(&self.current_path)),
            ("awayPaths", phab_core::php::round_trip(&self.away_paths)),
            (
                "oldProperties",
                phab_core::php::round_trip(&self.old_properties),
            ),
            (
                "newProperties",
                phab_core::php::round_trip(&self.new_properties),
            ),
            ("type", Value::from(self.change_type)),
            ("fileType", Value::from(self.file_type)),
            ("commitHash", Value::Null),
            (
                "hunks",
                Value::Array(self.hunks.iter().map(RawHunk::to_dictionary).collect()),
            ),
        ] {
            dictionary.insert(key.to_owned(), value);
        }
        Value::Object(dictionary)
    }
}

impl Default for RawChange {
    fn default() -> Self {
        Self {
            metadata: Some(Map::new()),
            old_path: None,
            current_path: None,
            away_paths: Value::Array(Vec::new()),
            old_properties: Value::Array(Vec::new()),
            new_properties: Value::Array(Vec::new()),
            change_type: change_type::CHANGE,
            file_type: FILE_TEXT,
            hunks: Vec::new(),
        }
    }
}

/// Why a raw diff was not parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// `Can't parse an empty diff!`
    Empty,
    /// What PHP does is not ported, or PHP fails with a message quoting the
    /// input.
    Unsupported(&'static str),
}

/// `filterPath()`.
fn filter_path(path: String) -> Option<String> {
    (path != "/dev/null").then_some(path)
}

/// `unescapeFilename()`: a name in quotes (`/^".+"$/`) loses them and its
/// C escapes (`stripcslashes()`). Names which are not UTF-8 then are left
/// to PHP.
fn unescape_filename(name: &str) -> Result<String, ParseError> {
    let bytes = name.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return Ok(name.to_owned());
    }
    String::from_utf8(stripcslashes(&bytes[1..bytes.len() - 1]))
        .map_err(|_| ParseError::Unsupported("escaped file names which are not UTF-8"))
}

/// PHP's `stripcslashes()`.
fn stripcslashes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        match bytes[i] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(0x07),
            b'v' => out.push(0x0b),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'x' if bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit) => {
                let mut value = 0u32;
                let mut taken = 0;
                while taken < 2 && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit) {
                    i += 1;
                    value = value * 16 + char::from(bytes[i]).to_digit(16).unwrap_or(0);
                    taken += 1;
                }
                out.push((value & 0xff) as u8);
            }
            b'0'..=b'7' => {
                let mut value = u32::from(bytes[i] - b'0');
                let mut taken = 1;
                while taken < 3
                    && bytes
                        .get(i + 1)
                        .is_some_and(|byte| (b'0'..=b'7').contains(byte))
                {
                    i += 1;
                    value = value * 8 + u32::from(bytes[i] - b'0');
                    taken += 1;
                }
                out.push((value & 0xff) as u8);
            }
            other => out.push(other),
        }
        i += 1;
    }
    out
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a valid pattern"))
}

fn is_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text.as_bytes()).unwrap_or(false)
}

/// `extractGitCommonFilename()`.
fn git_common_filename(paths: &str) -> Result<Option<String>, ParseError> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = regex(
        &PATTERN,
        r#"^(?P<old>(?P<oldq>"?)(?:[^/]+/)?(?P<common>.+)\k<oldq>) (?P<new>(?P<newq>"?)(?:[^/]+/)?\k<common>\k<newq>)$"#,
    );
    let paths = paths.trim_end_matches(['\r', '\n']);
    let Some(captures) = pattern.captures(paths.as_bytes()).ok().flatten() else {
        return Ok(None);
    };
    let group = |name| {
        captures
            .name(name)
            .map(|found| String::from_utf8_lossy(found.as_bytes()).into_owned())
            .unwrap_or_default()
    };
    let quote = group("newq");
    unescape_filename(&format!("{quote}{}{quote}", group("common"))).map(Some)
}

/// `trim($line, "\r\n")`.
fn trim_newlines(line: &str) -> &str {
    line.trim_matches(['\r', '\n'])
}

/// PHP's `trim()` and `rtrim()` default characters.
const WHITESPACE: [char; 6] = [' ', '\t', '\n', '\r', '\0', '\x0B'];

/// The parser's state: the lines and the changes found so far, by path
/// (`$this->changes`).
struct Parser {
    lines: Vec<String>,
    line: usize,
    changes: Vec<RawChange>,
    keys: Vec<Option<String>>,
}

/// A line pattern of `parseIndexHunk()`.
enum Header {
    NewFile(String),
    Deleted(String),
    OldMode(String),
    NewMode(String),
    Similarity,
    RenameFrom(String),
    RenameTo(String),
    CopyFrom(String),
    CopyTo(String),
}

/// `\d+` at the start of `text`.
fn leading_digits(text: &str) -> Option<String> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

/// `.*` at the start of `text`: up to the first newline.
fn rest_of_line(text: &str) -> String {
    text.split('\n').next().unwrap_or_default().to_owned()
}

fn header(line: &str) -> Option<Header> {
    if let Some(rest) = line.strip_prefix("new file mode ")
        && let Some(mode) = leading_digits(rest)
    {
        return Some(Header::NewFile(mode));
    }
    if let Some(rest) = line.strip_prefix("deleted file mode ")
        && let Some(mode) = leading_digits(rest)
    {
        return Some(Header::Deleted(mode));
    }
    if let Some(rest) = line.strip_prefix("old mode ")
        && let Some(mode) = leading_digits(rest)
    {
        return Some(Header::OldMode(mode));
    }
    if let Some(rest) = line.strip_prefix("new mode ")
        && let Some(mode) = leading_digits(rest)
    {
        return Some(Header::NewMode(mode));
    }
    if line.starts_with("similarity index ") {
        return Some(Header::Similarity);
    }
    if let Some(rest) = line.strip_prefix("rename from ") {
        return Some(Header::RenameFrom(rest_of_line(rest)));
    }
    if let Some(rest) = line.strip_prefix("rename to ") {
        return Some(Header::RenameTo(rest_of_line(rest)));
    }
    if let Some(rest) = line.strip_prefix("copy from ") {
        return Some(Header::CopyFrom(rest_of_line(rest)));
    }
    if let Some(rest) = line.strip_prefix("copy to ") {
        return Some(Header::CopyTo(rest_of_line(rest)));
    }
    None
}

/// PHP's `empty()` of a string.
fn is_empty_string(text: &str) -> bool {
    text.is_empty() || text == "0"
}

/// A number of a hunk header as PHP keeps it: digits which stay the same
/// as an integer. Leading zeros and numbers a `uint32` column refuses are
/// left to PHP.
fn header_number(digits: &str) -> Result<i64, ParseError> {
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(ParseError::Unsupported("hunk headers with leading zeros"));
    }
    digits
        .parse::<u32>()
        .map(i64::from)
        .map_err(|_| ParseError::Unsupported("hunk header numbers beyond 32 bits"))
}

/// `@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: .*?)?$`.
fn hunk_header(line: &str) -> Option<(String, Option<String>, String, Option<String>)> {
    fn number(text: &str) -> Option<(String, &str)> {
        let digits = leading_digits(text)?;
        let rest = &text[digits.len()..];
        Some((digits, rest))
    }
    fn length(text: &str) -> (Option<String>, &str) {
        if let Some(rest) = text.strip_prefix(',')
            && let Some((digits, rest)) = number(rest)
        {
            return (Some(digits), rest);
        }
        (None, text)
    }
    let rest = line.strip_prefix("@@ -")?;
    let (old_offset, rest) = number(rest)?;
    let (old_len, rest) = length(rest);
    let rest = rest.strip_prefix(" +")?;
    let (new_offset, rest) = number(rest)?;
    let (new_len, rest) = length(rest);
    let rest = rest.strip_prefix(" @@")?;
    if !(rest.is_empty() || rest.starts_with(' ') && !rest.contains('\n')) {
        return None;
    }
    Some((old_offset, old_len, new_offset, new_len))
}

/// A hunk length as PHP counts it down: missing means one line.
fn header_length(length: Option<String>) -> Result<i64, ParseError> {
    match length {
        None => Ok(1),
        Some(digits) => header_number(&digits),
    }
}

impl Parser {
    fn get_line(&self) -> Option<&str> {
        self.lines.get(self.line).map(String::as_str)
    }

    fn get_line_owned(&self) -> Option<String> {
        self.get_line().map(str::to_owned)
    }

    fn next_line(&mut self) -> Option<String> {
        self.line += 1;
        self.get_line_owned()
    }

    fn next_nonempty_line(&mut self) -> Option<String> {
        while let Some(line) = self.next_line() {
            if !line.trim_matches(WHITESPACE).is_empty() {
                break;
            }
        }
        self.get_line_owned()
    }

    /// `buildChange()`: the index of the change of `path`, created if new.
    fn build_change(&mut self, path: Option<&str>) -> usize {
        if let Some(path) = path
            && let Some(index) = self
                .keys
                .iter()
                .position(|key| key.as_deref() == Some(path))
        {
            return index;
        }
        let change = RawChange {
            current_path: path.map(str::to_owned).and_then(filter_path),
            ..RawChange::default()
        };
        self.changes.push(change);
        self.keys.push(path.map(str::to_owned));
        self.changes.len() - 1
    }

    /// The main loop of `parseDiff()`, for `diff --git` headers only.
    fn parse(&mut self) -> Result<(), ParseError> {
        loop {
            let line = trim_newlines(self.get_line().unwrap_or_default()).to_owned();
            let Some(paths) = line.strip_prefix("diff --git ") else {
                return Err(ParseError::Unsupported("raw diffs which are not Git diffs"));
            };
            let filename = git_common_filename(paths)?;
            let index = self.build_change(filename.as_deref());
            if let Some(filename) = filename {
                self.changes[index].old_path = filter_path(filename);
            }
            self.next_line();
            self.parse_index_hunk(index)?;
            if self.get_line().is_none() {
                return Ok(());
            }
        }
    }

    /// `parseIndexHunk()` for Git.
    fn parse_index_hunk(&mut self, index: usize) -> Result<(), ParseError> {
        static BINARY_DIFF: OnceLock<Regex> = OnceLock::new();
        static BINARY_CHANGED: OnceLock<Regex> = OnceLock::new();
        let mut line = self.get_line_owned();
        loop {
            let found = line.as_deref().and_then(header);
            let Some(found) = found else {
                if line.as_deref().is_none_or(|line| {
                    line.starts_with("diff --git ") || line.starts_with("commit ")
                }) {
                    // Only mode changes, or a pure move.
                    return Ok(());
                }
                break;
            };
            let change = &mut self.changes[index];
            match found {
                Header::NewFile(mode) => {
                    set_mode(&mut change.new_properties, &mode);
                    change.change_type = if change.change_type == change_type::DELETE {
                        change_type::CHANGE
                    } else {
                        change_type::ADD
                    };
                }
                Header::Deleted(mode) => {
                    set_mode(&mut change.old_properties, &mode);
                    change.change_type = change_type::DELETE;
                }
                Header::OldMode(mode) => set_mode(&mut change.old_properties, &mode),
                Header::NewMode(mode) => set_mode(&mut change.new_properties, &mode),
                Header::Similarity => {}
                Header::RenameFrom(old) | Header::CopyFrom(old) => {
                    if !is_empty_string(&old) {
                        change.old_path = filter_path(unescape_filename(&old)?);
                    }
                }
                Header::RenameTo(current) => {
                    if !is_empty_string(&current) {
                        change.current_path = filter_path(unescape_filename(&current)?);
                    }
                    self.moved(index, false);
                }
                Header::CopyTo(current) => {
                    if !is_empty_string(&current) {
                        change.current_path = filter_path(unescape_filename(&current)?);
                    }
                    self.moved(index, true);
                }
            }
            line = self.next_nonempty_line();
        }

        let mut line = self.get_line_owned();
        if line
            .as_deref()
            .is_some_and(|line| line.starts_with("index "))
        {
            line = self.next_nonempty_line();
        }
        let Some(line) = line else {
            return Ok(());
        };
        if ["Index: ", "Property changes on: ", "diff --git ", "commit "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(WHITESPACE);
        if trimmed == "Cannot display: file marked as a binary type." {
            self.next_line();
            self.next_nonempty_line();
            self.changes[index].file_type = FILE_BINARY;
            return Ok(());
        }
        if is_match(
            regex(&BINARY_DIFF, r"^(Binary files|Files) .* and .* differ$"),
            trimmed,
        ) || is_match(
            regex(&BINARY_CHANGED, r"^Binary file .* has changed$"),
            trimmed,
        ) {
            self.next_nonempty_line();
            self.changes[index].file_type = FILE_BINARY;
            return Ok(());
        }
        if trimmed == "GIT binary patch" {
            self.next_line();
            self.parse_git_binary_patch()?;
            if self
                .get_line()
                .is_some_and(|line| line.starts_with("literal"))
            {
                self.parse_git_binary_patch()?;
            }
            self.changes[index].file_type = FILE_BINARY;
            return Ok(());
        }
        let old_file = self.parse_hunk_target()?;
        self.parse_hunk_target()?;
        self.changes[index].old_path = filter_path(old_file);
        self.parse_changeset(index)
    }

    /// The `copy` and `move` cases of `parseIndexHunk()`: the change
    /// `index` was copied or moved from its old path.
    fn moved(&mut self, index: usize, copy: bool) {
        self.changes[index].change_type = if copy {
            change_type::COPY_HERE
        } else {
            change_type::MOVE_HERE
        };
        let old_path = self.changes[index].old_path.clone();
        let current = self.changes[index].current_path.clone();
        let old = self.build_change(old_path.as_deref());
        let source = &mut self.changes[old];
        source.change_type = match (copy, source.change_type) {
            (true, change_type::MOVE_AWAY) => change_type::MULTICOPY,
            (true, _) => change_type::COPY_AWAY,
            (false, change_type::MULTICOPY | change_type::MOVE_AWAY | change_type::COPY_AWAY) => {
                change_type::MULTICOPY
            }
            (false, _) => change_type::MOVE_AWAY,
        };
        if let Value::Array(paths) = &mut source.away_paths {
            paths.push(current.map_or(Value::Null, Value::String));
        }
    }

    /// `parseGitBinaryPatch()`.
    fn parse_git_binary_patch(&mut self) -> Result<(), ParseError> {
        if !self
            .get_line()
            .is_some_and(|line| line.starts_with("literal "))
        {
            return Err(ParseError::Unsupported("malformed Git binary patches"));
        }
        loop {
            let line = self.next_line();
            let trimmed = line.as_deref().map(trim_newlines);
            match trimmed {
                None | Some("") => {
                    self.next_nonempty_line();
                    return Ok(());
                }
                Some(text) if !text.starts_with(|c: char| c.is_ascii_alphabetic()) => {
                    return Err(ParseError::Unsupported("malformed Git binary patches"));
                }
                Some(_) => {}
            }
        }
    }

    /// `parseHunkTarget()` for Git.
    fn parse_hunk_target(&mut self) -> Result<String, ParseError> {
        static TARGET: OnceLock<Regex> = OnceLock::new();
        let pattern = regex(&TARGET, r"^[-+]{3} (?:[ab]/)?(?P<path>.*?)(?:\t.*)?$");
        let line = self
            .get_line()
            .ok_or(ParseError::Unsupported("diffs without hunk targets"))?;
        let path = pattern
            .captures(line.as_bytes())
            .ok()
            .flatten()
            .and_then(|captures| {
                captures
                    .name("path")
                    .map(|path| String::from_utf8_lossy(path.as_bytes()).into_owned())
            })
            .ok_or(ParseError::Unsupported("diffs without hunk targets"))?;
        self.next_line();
        Ok(path)
    }

    /// `parseChangeset()`.
    fn parse_changeset(&mut self, index: usize) -> Result<(), ParseError> {
        let unsupported = |what| Err(ParseError::Unsupported(what));
        self.changes[index].hunks.clear();
        loop {
            let header = self.get_line().map(trim_newlines).and_then(hunk_header);
            let Some((old_offset, old_len, new_offset, new_len)) = header else {
                return unsupported("diffs without hunk headers");
            };
            let lengths_given = (old_len.is_some(), new_len.is_some());
            let mut hunk = RawHunk {
                old_offset: header_number(&old_offset)?,
                old_length: header_length(old_len)?,
                new_offset: header_number(&new_offset)?,
                new_length: header_length(new_len)?,
                lengths_given,
                ..RawHunk::default()
            };
            let mut old_len = hunk.old_length;
            let mut new_len = hunk.new_length;
            let mut real = String::new();
            let mut hit_next_hunk = false;
            while let Some(line) = self.next_line() {
                let marker = if line.trim_end_matches(['\r', '\n']).is_empty() {
                    b' '
                } else {
                    line.as_bytes()[0]
                };
                match marker {
                    b'\\' => {
                        if !line.contains(" No newline at end of file") {
                            return unsupported("malformed no-newline markers");
                        }
                        real.push_str(&line);
                        if new_len == 0 {
                            hunk.is_missing_new_newline = true;
                            break;
                        }
                        hunk.is_missing_old_newline = true;
                    }
                    b'+' => {
                        hunk.add_lines += 1;
                        new_len -= 1;
                        real.push_str(&line);
                    }
                    b'-' => {
                        if old_len == 0 {
                            hit_next_hunk = true;
                            break;
                        }
                        hunk.del_lines += 1;
                        old_len -= 1;
                        real.push_str(&line);
                    }
                    b' ' => {
                        if old_len == 0 && new_len == 0 {
                            break;
                        }
                        old_len -= 1;
                        new_len -= 1;
                        real.push_str(&line);
                    }
                    _ => {
                        hit_next_hunk = true;
                        break;
                    }
                }
            }
            if old_len != 0 || new_len != 0 {
                return unsupported("hunks with the wrong number of lines");
            }
            hunk.corpus = real;
            self.changes[index].hunks.push(hunk);
            let line = if hit_next_hunk {
                self.get_line_owned()
            } else {
                self.next_nonempty_line()
            };
            if !line.is_some_and(|line| line.starts_with("@@ ")) {
                return Ok(());
            }
        }
    }
}

fn set_mode(properties: &mut Value, mode: &str) {
    if is_empty_string(mode) {
        return;
    }
    let mut map = match std::mem::take(properties) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    map.insert("unix:filemode".to_owned(), Value::String(mode.to_owned()));
    *properties = Value::Object(map);
}

/// `ArcanistDiffParser::parseDiff()` without a repository, for Git diffs.
pub fn parse_diff(diff: &str) -> Result<Vec<RawChange>, ParseError> {
    Ok(parse_diff_keyed(diff)?
        .into_iter()
        .map(|(_, change)| change)
        .collect())
}

/// [`parse_diff()`] with the key of each change in the array PHP returns:
/// its path, or `None` for a change PHP appends without one.
pub fn parse_diff_keyed(diff: &str) -> Result<Vec<(Option<String>, RawChange)>, ParseError> {
    if diff.trim_matches(WHITESPACE).is_empty() {
        return Err(ParseError::Empty);
    }
    // `git format-patch` output is stripped of its mail first; any line
    // `^---$` may start one.
    if lines::split_lines(diff)
        .iter()
        .any(|line| *line == "---\n" || *line == "---")
    {
        return Err(ParseError::Unsupported("diffs which may be patch mails"));
    }
    let text = diff.trim_start_matches(WHITESPACE);
    let colored = lines::split_lines(text).iter().any(|line| {
        line.strip_prefix("\x1b[").is_some_and(|rest| {
            rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == ';')
                .starts_with('m')
        })
    });
    if colored {
        return Err(ParseError::Unsupported("colorized diffs"));
    }
    let mut parser = Parser {
        lines: lines::split_lines(text)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        line: 0,
        changes: Vec::new(),
        keys: Vec::new(),
    };
    if parser.get_line().is_some_and(|line| line.starts_with('#')) {
        return Err(ParseError::Unsupported("diffs with header comments"));
    }
    // Paths which are integers would share PHP's array keys with the
    // changes without a path.
    parser.parse()?;
    if parser.keys.iter().flatten().any(|key| {
        !key.is_empty()
            && key
                .trim_start_matches('-')
                .bytes()
                .all(|byte| byte.is_ascii_digit())
    }) {
        return Err(ParseError::Unsupported("paths which are integers"));
    }
    Ok(parser.keys.into_iter().zip(parser.changes).collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_a_change() {
        let changes = parse_diff(
            "diff --git a/README b/README\nindex 1111111..2222222 100644\n--- a/README\n\
             +++ b/README\n@@ -1,2 +1,2 @@\n context\n-old\n+new\n",
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert_eq!(change.current_path.as_deref(), Some("README"));
        assert_eq!(change.old_path.as_deref(), Some("README"));
        assert_eq!(change.change_type, change_type::CHANGE);
        assert_eq!(
            change.hunks,
            [RawHunk {
                old_offset: 1,
                old_length: 2,
                new_offset: 1,
                new_length: 2,
                add_lines: 1,
                del_lines: 1,
                corpus: " context\n-old\n+new\n".to_owned(),
                lengths_given: (true, true),
                ..RawHunk::default()
            }]
        );
    }

    #[test]
    fn parses_adds_moves_and_binaries() {
        let changes = parse_diff(
            "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n\
             --- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n\
             diff --git a/old.txt b/moved.txt\nsimilarity index 100%\nrename from old.txt\n\
             rename to moved.txt\n\
             diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and \
             b/img.png differ\n",
        )
        .unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].change_type, change_type::ADD);
        assert_eq!(changes[0].old_path, None);
        assert_eq!(
            changes[0].new_properties,
            json!({"unix:filemode": "100644"})
        );
        assert_eq!(changes[0].hunks[0].new_length, 1);
        assert_eq!(changes[1].change_type, change_type::MOVE_HERE);
        assert_eq!(changes[1].current_path.as_deref(), Some("moved.txt"));
        assert_eq!(changes[1].old_path.as_deref(), Some("old.txt"));
        assert_eq!(changes[2].change_type, change_type::MOVE_AWAY);
        assert_eq!(changes[2].current_path.as_deref(), Some("old.txt"));
        assert_eq!(changes[2].away_paths, json!(["moved.txt"]));
        assert_eq!(changes[3].file_type, FILE_BINARY);
        assert_eq!(changes[3].hunks, []);
    }

    #[test]
    fn missing_newlines_end_hunks() {
        let changes = parse_diff(
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n\\ No newline at end of \
             file\n+b\n\\ No newline at end of file\n",
        )
        .unwrap();
        assert_eq!(
            changes[0].hunks[0].corpus,
            "-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n"
        );
        let hunk = changes[0].hunks[0].to_dictionary();
        assert_eq!(hunk["oldOffset"], json!("1"));
        assert_eq!(hunk["oldLength"], json!(1));
        assert_eq!(hunk["isMissingOldNewline"], json!(true));
        assert_eq!(hunk["isMissingNewNewline"], json!(true));
    }

    #[test]
    fn leaves_other_diffs_to_php() {
        assert_eq!(parse_diff(" \n"), Err(ParseError::Empty));
        for diff in [
            "Index: x\n===\n",
            "--- a/x\t2020-01-01\n+++ b/x\t2020-01-01\n@@ -1 +1 @@\n-a\n+b\n",
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n",
            "diff --git a/12 b/12\n--- a/12\n+++ b/12\n@@ -1 +1 @@\n-a\n+b\n",
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -01 +1 @@\n-a\n+b\n",
        ] {
            assert!(
                matches!(parse_diff(diff), Err(ParseError::Unsupported(_))),
                "{diff}"
            );
        }
    }

    #[test]
    fn unescapes_quoted_names() {
        assert_eq!(
            git_common_filename(r#""a/caf\303\251" "b/caf\303\251""#)
                .unwrap()
                .as_deref(),
            Some("café")
        );
        assert_eq!(git_common_filename("a/x b/y"), Ok(None));
        assert_eq!(stripcslashes(br"a\tb\x41\101\q"), b"a\tbAAq");
    }
}
