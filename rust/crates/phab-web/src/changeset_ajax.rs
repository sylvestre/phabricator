//! The Ajax answer of `/differential/changeset/`, which PHP's revision
//! and diff pages load each changeset and its "show more" context with
//! (`DifferentialChangesetParser::newChangesetResponse()`): the side by
//! side markup of `DifferentialChangesetTwoUpRenderer`, its state and
//! its Javelin metadata, as PHP's JSON.
//!
//! PHP highlights the files with Pygments, which Rust doesn't run: Rust
//! answers from the parse cache PHP stored when it rendered the changeset
//! (`differential_changeset_parse_cache`), whose lines are highlighted
//! already, and never writes it. What the cache can't answer is left to
//! PHP (`421`), before anything is written: no cache yet, other versions
//! of it, view state the request changes or the cache doesn't hold
//! (highlighting, encodings, document engines), comparisons between two
//! changesets, inline comments, lint messages and coverage, document
//! engines (Jupyter notebooks, JSON), property changes, binary and other
//! non-text changesets, the unified renderer, and text which isn't UTF-8.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use md5::{Digest as _, Md5};
use phab_db::phpser::{self, Key, Value as Php};
use phab_diff::{ChangeType, Changeset};
use serde_json::{Map, Value, json};

use crate::diff_html::escape;
use crate::error::PageError;

/// `DifferentialChangesetParser::CACHE_VERSION`.
const CACHE_VERSION: i64 = 14;

/// `DifferentialChangesetParser::HIGHLIGHT_BYTE_LIMIT`.
const HIGHLIGHT_BYTE_LIMIT: u64 = 262_144;

/// Changesets with more changed lines are behind a shield.
const LARGE_CHANGE_LINES: i64 = 2500;

/// `getCacheableProperties()`: a cache without one of them is ignored.
const CACHEABLE_PROPERTIES: [&str; 12] = [
    "visible",
    "new",
    "old",
    "intra",
    "depthOnlyLines",
    "newRender",
    "oldRender",
    "specialAttributes",
    "hunkStartLines",
    "cacheVersion",
    "cacheHost",
    "highlightingDisabled",
];

/// `renderShowContextLinks()`'s block size.
const BLOCK_SIZE: i64 = 20;

fn unsupported(reason: &'static str) -> PageError {
    PageError::Fallback(reason)
}

/// A rendered line: HTML (`PhutilSafeHTML`) or text, which markup
/// escapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Html {
    Safe(String),
    Text(String),
}

impl Html {
    fn markup(&self) -> String {
        match self {
            Self::Safe(html) => html.clone(),
            Self::Text(text) => escape(text),
        }
    }
}

/// A line of one side (`DifferentialHunkParser` line data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineSpec {
    /// `type`: `+`, `-`, `\`, or empty for context.
    kind: Option<String>,
    line: Option<i64>,
    text: String,
}

impl LineSpec {
    /// `if ($line['type'])`.
    fn has_type(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|kind| !kind.is_empty() && kind != "0")
    }

    fn is_no_newline(&self) -> bool {
        self.kind.as_deref() == Some("\\")
    }
}

/// The cached parse of a changeset (`getCacheableProperties()`).
#[derive(Debug, Clone, Default)]
pub(crate) struct ParseCache {
    visible: HashSet<i64>,
    /// The rows of each side, `None` where the side has no line; the
    /// length is `count()`.
    old: Vec<(i64, Option<LineSpec>)>,
    new: Vec<(i64, Option<LineSpec>)>,
    depth_only: HashMap<i64, String>,
    old_render: Option<HashMap<i64, Html>>,
    new_render: Option<HashMap<i64, Html>>,
    special: HashMap<String, bool>,
    /// The old line numbers hunks start at, in their order.
    hunk_starts: Vec<i64>,
    highlighting_disabled: bool,
}

fn utf8(bytes: &[u8]) -> Result<String, PageError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| unsupported("changesets which aren't UTF-8"))
}

fn int_key(key: &Key) -> Result<i64, PageError> {
    match key {
        Key::Int(value) => Ok(*value),
        Key::Str(_) => Err(unsupported("an unusual parse cache")),
    }
}

fn entries(value: &Php) -> Result<&[(Key, Php)], PageError> {
    match value {
        Php::Array(entries) => Ok(entries),
        _ => Err(unsupported("an unusual parse cache")),
    }
}

/// PHP's truthiness of a cached scalar.
fn truthy(value: &Php) -> bool {
    match value {
        Php::Null => false,
        Php::Bool(flag) => *flag,
        Php::Int(value) => *value != 0,
        Php::Float(value) => *value != 0.0,
        Php::Str(bytes) => !bytes.is_empty() && bytes != b"0",
        Php::Array(entries) => !entries.is_empty(),
        Php::Object { .. } => true,
    }
}

fn line_spec(value: &Php) -> Result<Option<LineSpec>, PageError> {
    if *value == Php::Null {
        return Ok(None);
    }
    let field = |name: &str| value.get(&Key::from(name));
    let kind = match field("type") {
        None | Some(Php::Null) => None,
        Some(Php::Str(bytes)) => Some(utf8(bytes)?),
        Some(_) => return Err(unsupported("an unusual parse cache")),
    };
    let line = match field("line") {
        None | Some(Php::Null) => None,
        Some(Php::Int(line)) => Some(*line),
        Some(_) => return Err(unsupported("an unusual parse cache")),
    };
    let text = match field("text") {
        Some(Php::Str(bytes)) => utf8(bytes)?,
        None | Some(Php::Null) => String::new(),
        Some(_) => return Err(unsupported("an unusual parse cache")),
    };
    Ok(Some(LineSpec { kind, line, text }))
}

fn lines(value: &Php) -> Result<Vec<(i64, Option<LineSpec>)>, PageError> {
    entries(value)?
        .iter()
        .map(|(key, value)| Ok((int_key(key)?, line_spec(value)?)))
        .collect()
}

fn render_lines(value: &Php) -> Result<Option<HashMap<i64, Html>>, PageError> {
    if *value == Php::Null {
        return Ok(None);
    }
    let safe_content = phpser::private("PhutilSafeHTML", "content");
    let mut render = HashMap::new();
    for (key, line) in entries(value)? {
        let html = match line {
            Php::Str(bytes) => Html::Text(utf8(bytes)?),
            Php::Object { class, properties } if class == "PhutilSafeHTML" => {
                let content = properties
                    .iter()
                    .find(|(name, _)| *name == safe_content)
                    .and_then(|(_, value)| value.as_bytes())
                    .ok_or(unsupported("an unusual parse cache"))?;
                Html::Safe(utf8(content)?)
            }
            Php::Null => continue,
            _ => return Err(unsupported("an unusual parse cache")),
        };
        render.insert(int_key(key)?, html);
    }
    Ok(Some(render))
}

impl ParseCache {
    /// `loadCache()` of a stored cache for a top-level render
    /// (`top_level`) without inline comments; `None` when PHP would
    /// reparse the changeset.
    pub(crate) fn decode(data: &[u8], top_level: bool) -> Result<Option<Self>, PageError> {
        // Old JSON caches are ignored.
        if data.first() == Some(&b'{') {
            return Ok(None);
        }
        let Ok(value) = phpser::unserialize_objects(data) else {
            return Ok(None);
        };
        let Php::Array(found) = &value else {
            return Ok(None);
        };
        if found.is_empty() {
            return Ok(None);
        }
        let field = |name: &str| value.get(&Key::from(name));
        if CACHEABLE_PROPERTIES.iter().any(|key| field(key).is_none()) {
            return Ok(None);
        }
        if field("cacheVersion") != Some(&Php::Int(CACHE_VERSION)) {
            return Ok(None);
        }
        let new_render = field("newRender").unwrap_or(&Php::Null);
        // A partial cache of a shielded file.
        if *new_render == Php::Null && !top_level {
            return Ok(None);
        }
        let empty = Php::Array(Vec::new());
        let get = |name: &str| field(name).unwrap_or(&empty);
        // `isset()`: any value but `null`.
        let visible = entries(get("visible"))?
            .iter()
            .filter(|(_, shown)| *shown != Php::Null)
            .map(|(key, _)| int_key(key))
            .collect::<Result<_, _>>()?;
        let depth_only = entries(get("depthOnlyLines"))?
            .iter()
            .map(|(key, value)| {
                let kind = value
                    .as_bytes()
                    .ok_or(unsupported("an unusual parse cache"))?;
                Ok((int_key(key)?, utf8(kind)?))
            })
            .collect::<Result<_, PageError>>()?;
        let special = entries(get("specialAttributes"))?
            .iter()
            .map(|(key, value)| match key {
                Key::Str(name) => Ok((utf8(name)?, truthy(value))),
                Key::Int(_) => Err(unsupported("an unusual parse cache")),
            })
            .collect::<Result<_, PageError>>()?;
        let hunk_starts = entries(get("hunkStartLines"))?
            .iter()
            .map(|(key, _)| int_key(key))
            .collect::<Result<_, _>>()?;
        Ok(Some(Self {
            visible,
            old: lines(get("old"))?,
            new: lines(get("new"))?,
            depth_only,
            old_render: render_lines(field("oldRender").unwrap_or(&Php::Null))?,
            new_render: render_lines(new_render)?,
            special,
            hunk_starts,
            highlighting_disabled: field("highlightingDisabled").is_some_and(truthy),
        }))
    }

    fn special(&self, name: &str) -> bool {
        self.special.get(name).copied().unwrap_or(false)
    }
}

/// The Javelin metadata of the answer (`CelerityStaticResourceResponse`):
/// `data-meta` IDs are `<block>_<index>`.
#[derive(Debug, Default)]
pub(crate) struct Metadata {
    block: i64,
    entries: Vec<Value>,
}

impl Metadata {
    pub(crate) fn new(block: i64) -> Self {
        Self {
            block,
            entries: Vec::new(),
        }
    }

    fn add(&mut self, value: Value) -> String {
        let id = format!("{}_{}", self.block, self.entries.len());
        self.entries.push(value);
        id
    }
}

/// What the request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    /// `ref`, as given.
    pub(crate) reference: String,
    /// `parseRangeSpecification()` of `range`.
    pub(crate) range: (Option<i64>, Option<i64>, Vec<i64>),
}

/// `parseRangeSpecification()`: `start-length`, then an optional
/// `start-length` of rows to show.
pub(crate) fn parse_range(spec: Option<&str>) -> (Option<i64>, Option<i64>, Vec<i64>) {
    let Some(spec) = spec.filter(|spec| !spec.is_empty() && *spec != "0") else {
        return (None, None, Vec::new());
    };
    let pair = |part: &str| -> Option<(i64, i64)> {
        let (start, len) = part.split_once('-')?;
        let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        if !digits(start) || !digits(len) {
            return None;
        }
        Some((start.parse().ok()?, len.parse().ok()?))
    };
    let (range, mask) = match spec.split_once('/') {
        Some((range, mask)) => (range, Some(mask)),
        None => (spec, None),
    };
    let Some((start, len)) = pair(range) else {
        return (None, None, Vec::new());
    };
    let mask = match mask.map(pair) {
        None => Vec::new(),
        Some(None) => return (None, None, Vec::new()),
        Some(Some((from, length))) => (from..from + length).collect(),
    };
    (Some(start), Some(len), mask)
}

/// `PhutilNumber` in English: digits grouped by thousands.
fn number(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// `basename()` of a path.
fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// `dirname()` of an absolute path.
fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => trimmed[..index].trim_end_matches('/').to_owned(),
    }
}

/// The side by side renderer, for one changeset shown alone (no
/// comparison): the left side is its old file, the right its new one.
pub(crate) struct TwoUp<'a> {
    changeset: &'a Changeset,
    cache: &'a ParseCache,
    reference: &'a str,
    /// The encoding of the first hunk (`getDataEncoding()`).
    encoding: Option<&'a str>,
    is_top_level: bool,
    is_undershield: bool,
    gaps: Vec<(i64, i64)>,
    mask: HashSet<i64>,
}

impl TwoUp<'_> {
    fn copy_lines(&self) -> Result<HashMap<i64, (String, i64, String)>, PageError> {
        let Some(lines) = self.changeset.metadata.get("copy:lines") else {
            return Ok(HashMap::new());
        };
        let unusual = || unsupported("unusual copied lines");
        let map = match lines {
            Value::Object(map) => map,
            Value::Array(list) if list.is_empty() => return Ok(HashMap::new()),
            _ => return Err(unusual()),
        };
        let mut copied = HashMap::new();
        for (line, spec) in map {
            let line: i64 = line.parse().map_err(|_| unusual())?;
            let Some([file, orig_line, kind]) = spec.as_array().map(Vec::as_slice) else {
                return Err(unusual());
            };
            let orig_line = match orig_line {
                Value::Number(number) => number.as_i64().ok_or_else(unusual)?,
                Value::String(text) => text.parse().map_err(|_| unusual())?,
                _ => return Err(unusual()),
            };
            let (Some(file), Some(kind)) = (file.as_str(), kind.as_str()) else {
                return Err(unusual());
            };
            copied.insert(line, (file.to_owned(), orig_line, kind.to_owned()));
        }
        Ok(copied)
    }

    fn line(side: &[(i64, Option<LineSpec>)], row: i64) -> Option<&LineSpec> {
        side.iter()
            .find(|(key, _)| *key == row)
            .and_then(|(_, spec)| spec.as_ref())
    }

    /// `getScopeEngine()`: none when context is missing.
    fn scope_engine(&self) -> Result<Option<ScopeEngine>, PageError> {
        if !self.cache.hunk_starts.is_empty() {
            return Ok(None);
        }
        let mut text_map = BTreeMap::new();
        for (_, spec) in &self.cache.new {
            if let Some(spec) = spec
                && let Some(line) = spec.line
            {
                text_map.insert(line, spec.text.clone());
            }
        }
        // PHP throws on a map which isn't `1..n`.
        if text_map
            .keys()
            .enumerate()
            .any(|(index, line)| i64::try_from(index + 1).ok() != Some(*line))
        {
            return Err(unsupported("changesets whose lines aren't contiguous"));
        }
        Ok(Some(ScopeEngine::new(
            text_map.into_values().collect::<Vec<_>>(),
        )))
    }

    fn render_show_context_link(
        meta: &mut Metadata,
        is_all: bool,
        range: &str,
        text: &str,
    ) -> String {
        let id = meta
            .add(json!({"type": if is_all { json!("all") } else { Value::Null }, "range": range}));
        format!(
            "<a href=\"#\" data-mustcapture=\"1\" data-sigil=\"show-more\" data-meta=\"{id}\">{}</a>",
            escape(text)
        )
    }

    /// `renderShowContextLinks()` for lines.
    fn render_show_context_links(
        meta: &mut Metadata,
        top: i64,
        len: i64,
        changeset_length: i64,
    ) -> String {
        let end = (top + len) - BLOCK_SIZE;
        let is_large_block = len > BLOCK_SIZE * 2;
        let block = number(BLOCK_SIZE);
        let mut links = Vec::new();
        if is_large_block {
            let text = if top == 0 {
                format!("Show First {block} Lines")
            } else {
                format!("\u{25B2} Show {block} Lines")
            };
            links.push(Self::render_show_context_link(
                meta,
                false,
                &format!("{top}-{len}/{top}-20"),
                &text,
            ));
        }
        let text = if len == 1 {
            "Show Line".to_owned()
        } else {
            format!("Show All {} Lines", number(len))
        };
        links.push(Self::render_show_context_link(
            meta,
            true,
            &format!("{top}-{len}/{top}-{len}"),
            &text,
        ));
        if is_large_block {
            let text = if top + len >= changeset_length {
                format!("Show Last {block} Lines")
            } else {
                format!("\u{25BC} Show {block} Lines")
            };
            links.push(Self::render_show_context_link(
                meta,
                false,
                &format!("{top}-{len}/{end}-20"),
                &text,
            ));
        }
        links.join(" \u{2022} ")
    }

    fn wrap_change_in_table(content: &str) -> String {
        format!(
            "<table class=\"differential-diff remarkup-code PhabricatorMonospaced diff-2up\" \
             data-sigil=\"differential-diff intercept-copy\"><colgroup><col class=\"num\" />\
             <col class=\"left\" /><col class=\"num\" /><col class=\"copy\" />\
             <col class=\"right\" /><col class=\"cov\" /></colgroup>{content}</table>"
        )
    }

    /// The "show more" row of the gap `(top, len)` at row `ii`.
    fn render_gap_row(
        &self,
        meta: &mut Metadata,
        scope_engine: Option<&ScopeEngine>,
        (ii, top, len): (i64, i64, i64),
        rows: i64,
    ) -> Result<String, PageError> {
        let cache = self.cache;
        let contents = Self::render_show_context_links(meta, top, len, rows);
        let is_last_block = ii + len >= rows;
        let mut context_line = None;
        let mut context_text = None;
        if !is_last_block && let Some(engine) = scope_engine {
            let target = Self::line(&cache.new, ii + len)
                .and_then(|spec| spec.line)
                .ok_or(unsupported("gaps before removed lines"))?;
            context_line = engine.scope_start(target);
            if let Some(line) = context_line {
                // `getNewLineToOffsetMap()`: the last row of the line.
                let offset = cache
                    .new
                    .iter()
                    .rev()
                    .find(|(_, spec)| spec.as_ref().and_then(|spec| spec.line) == Some(line))
                    .map(|(offset, _)| *offset)
                    .ok_or(unsupported("scopes PHP can't find"))?;
                let empty = HashMap::new();
                let new_render = cache.new_render.as_ref().unwrap_or(&empty);
                // `phutil_safe_html()` of either.
                context_text = match new_render.get(&offset) {
                    Some(Html::Safe(html) | Html::Text(html)) => Some(html.clone()),
                    None => return Err(unsupported("scopes PHP can't render")),
                };
            }
        }
        let data_n = context_line.map_or_else(String::new, |line| format!(" data-n=\"{line}\""));
        Ok(format!(
            "<tr data-sigil=\"context-target\"><td class=\"show-context-line n \
             left-context\"></td><td class=\"show-more\">{contents}</td><td \
             class=\"show-context-line n\"{data_n}></td><td colspan=\"3\" \
             class=\"show-context\">{}</td></tr>",
            context_text.unwrap_or_default()
        ))
    }

    /// The copy gutter of a changed line: whether it was moved or copied
    /// from elsewhere (`copy:lines`).
    fn copy_cell(
        meta: &mut Metadata,
        copied: Option<&(String, i64, String)>,
        not_copied: bool,
    ) -> String {
        let Some((orig_file, orig_line, orig_type)) = copied.filter(|_| !not_copied) else {
            return "<td class=\"copy\"></td>".to_owned();
        };
        let is_move = orig_type == "-";
        let mut title = if is_move {
            "Moved from ".to_owned()
        } else {
            "Copied from ".to_owned()
        };
        if orig_file.is_empty() {
            let _ = write!(title, "line {orig_line}");
        } else {
            let _ = write!(
                title,
                "{}:{orig_line} in dir {}",
                basename(orig_file),
                dirname(&format!("/{orig_file}"))
            );
        }
        let class = if is_move { "new-move" } else { "new-copy" };
        let id = meta.add(json!({"msg": title}));
        format!("<td class=\"copy {class}\" data-meta=\"{id}\"></td>")
    }

    /// The row of display row `ii`.
    fn render_line_row(
        &self,
        meta: &mut Metadata,
        copy_lines: &HashMap<i64, (String, i64, String)>,
        ii: i64,
    ) -> (Option<i64>, String) {
        let cache = self.cache;
        let empty = HashMap::new();
        let old_render = cache.old_render.as_ref().unwrap_or(&empty);
        let new_render = cache.new_render.as_ref().unwrap_or(&empty);
        let old_line = Self::line(&cache.old, ii);
        let new_line = Self::line(&cache.new, ii);

        let mut o_num = None;
        let mut o_classes = "";
        let mut o_text = None;
        if let Some(spec) = old_line {
            o_num = spec.line;
            o_text = old_render.get(&ii).map(Html::markup);
            if spec.has_type() {
                o_classes = if spec.is_no_newline() {
                    o_text = Some(escape(&spec.text));
                    "comment"
                } else if new_line.is_none() {
                    "old old-full"
                } else if cache.depth_only.get(&ii).map(String::as_str) == Some(">") {
                    ""
                } else {
                    "old"
                };
            }
        }

        let mut n_copy = "<td class=\"copy\" />".to_owned();
        let mut n_classes = "";
        let mut n_num = None;
        let mut n_text = None;
        if let Some(spec) = new_line {
            n_num = spec.line;
            n_text = new_render.get(&ii).map(Html::markup);
            if spec.has_type() {
                n_classes = if spec.is_no_newline() {
                    n_text = Some(escape(&spec.text));
                    "comment"
                } else if old_line.is_none() {
                    "new new-full"
                } else if cache.depth_only.contains_key(&ii) {
                    ""
                } else {
                    "new"
                };
                let copied = n_num.and_then(|line| copy_lines.get(&line));
                let not_copied =
                    copied.is_none() || cache.depth_only.contains_key(&ii) || spec.is_no_newline();
                n_copy = Self::copy_cell(meta, copied, not_copied);
            }
        }

        let id = self.changeset.id;
        let number_cell = |id: Option<String>, classes: &str, number: Option<i64>| {
            let id = id.map_or_else(String::new, |id| format!(" id=\"{id}\""));
            let data = number.map_or_else(String::new, |n| format!(" data-n=\"{n}\""));
            format!("<td{id} class=\"{classes} n\"{data}></td>")
        };
        let o_id = o_num
            .filter(|line| *line != 0)
            .map(|line| format!("C{id}OL{line}"));
        let n_id = n_num
            .filter(|line| *line != 0)
            .map(|line| format!("C{id}NL{line}"));
        let row = format!(
            "<tr>{}<td class=\"{o_classes}\" data-copy-mode=\"copy-l\">{}</td>{}{n_copy}<td \
             class=\"{n_classes}\" colspan=\"2\" data-copy-mode=\"copy-r\">{}</td></tr>",
            number_cell(o_id, o_classes, o_num),
            o_text.unwrap_or_default(),
            number_cell(n_id, n_classes, n_num),
            n_text.unwrap_or_default(),
        );
        (o_num, row)
    }

    /// `renderTextChange()`.
    fn render_text_change(
        &self,
        meta: &mut Metadata,
        range_start: i64,
        range_len: i64,
        rows: i64,
    ) -> Result<String, PageError> {
        let cache = self.cache;
        let context_not_available = (!cache.hunk_starts.is_empty()).then_some(
            "<tr data-sigil=\"context-target\"><td colspan=\"6\" class=\"show-more\">Context \
             not available.</td></tr>",
        );
        let hunk_starts: HashSet<i64> = cache.hunk_starts.iter().copied().collect();
        let copy_lines = self.copy_lines()?;
        // PHP builds it for the first gap which isn't the last block; a
        // map it refuses then is refused here for any changeset.
        let scope_engine = self.scope_engine()?;
        let mut gaps = self.gaps.clone();
        let mut html = String::new();
        let mut ii = range_start;
        while ii < range_start + range_len {
            if !self.mask.contains(&ii) {
                let (top, len) = gaps
                    .pop()
                    .ok_or(unsupported("changesets whose gaps PHP can't render"))?;
                html.push_str(&self.render_gap_row(
                    meta,
                    scope_engine.as_ref(),
                    (ii, top, len),
                    rows,
                )?);
                ii += len;
                continue;
            }
            let (o_num, row) = self.render_line_row(meta, &copy_lines, ii);
            // `$o_num && isset($hunk_starts[$o_num])`.
            if let (Some(line), Some(context)) = (o_num, context_not_available)
                && line != 0
                && hunk_starts.contains(&line)
            {
                html.push_str(context);
            }
            html.push_str(&row);
            if let Some(context) = context_not_available
                && ii == rows - 1
            {
                html.push_str(context);
            }
            ii += 1;
        }
        Ok(Self::wrap_change_in_table(&html))
    }

    /// `renderShield()`.
    fn render_shield(&self, meta: &mut Metadata, message: &str, force: &str) -> String {
        let end = self.cache.old.len();
        let mut range = format!("0-{end}");
        if force == "text" {
            range = format!("{range}/0-{end}");
        }
        let mut content = escape(message);
        if force != "none" {
            let id = meta.add(json!({"ref": self.reference, "range": range}));
            let _ = write!(
                content,
                " <a class=\"complete\" href=\"#\" data-mustcapture=\"1\" \
                 data-sigil=\"show-more\" data-meta=\"{id}\">Show File Contents</a>"
            );
        }
        Self::wrap_change_in_table(&format!(
            "<tr data-sigil=\"context-target\"><td class=\"differential-shield\" \
             colspan=\"6\">{content}</td></tr>"
        ))
    }

    /// `renderChangeTypeHeader()` of a text file.
    fn render_change_type_header(&self, force: bool) -> String {
        let changeset = self.changeset;
        let strong = |text: &str| format!("<strong>{}</strong>", escape(text));
        let away = || strong(&changeset.away_paths.join(", "));
        let message = match changeset.change_type {
            ChangeType::Add => Some("This file was added.".to_owned()),
            ChangeType::Delete => Some("This file was deleted.".to_owned()),
            ChangeType::MoveHere => Some(format!(
                "This file was moved from {}.",
                strong(changeset.old_file.as_deref().unwrap_or(""))
            )),
            ChangeType::CopyHere => Some(format!(
                "This file was copied from {}.",
                strong(changeset.old_file.as_deref().unwrap_or(""))
            )),
            ChangeType::MoveAway => Some(format!("This file was moved to {}.", away())),
            ChangeType::CopyAway => Some(format!("This file was copied to {}.", away())),
            ChangeType::Multicopy => Some(format!(
                "This file was deleted after being copied to {}.",
                away()
            )),
            _ => force.then(|| "This file was not modified.".to_owned()),
        };
        format_header_messages(&message.into_iter().collect::<Vec<_>>())
    }

    /// `renderUndershieldHeader()` of a text file.
    fn render_undershield_header(&self) -> String {
        let mut messages = Vec::new();
        if !self.changeset.hunks.is_empty() && self.encoding != Some("utf8") {
            messages.push(match self.encoding {
                Some(encoding) if !encoding.is_empty() => format!(
                    "This file was converted from <strong>{}</strong> for display.",
                    escape(encoding)
                ),
                _ => "This file uses an unknown character encoding.".to_owned(),
            });
        }
        if self.cache.highlighting_disabled {
            messages.push(format!(
                "This file is larger than {}, so syntax highlighting is disabled by default.",
                escape(&phab_app_files::format_bytes(HIGHLIGHT_BYTE_LIMIT))
            ));
        }
        format_header_messages(&messages)
    }

    /// `renderChangesetTable()`.
    fn render_changeset_table(&self, content: &str) -> String {
        let mut html = String::new();
        if self.is_top_level {
            html.push_str(&self.render_change_type_header(content.is_empty()));
        }
        if self.is_undershield {
            html.push_str(&self.render_undershield_header());
        }
        html.push_str(content);
        html
    }
}

fn format_header_messages(messages: &[String]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut items = String::new();
    for message in messages {
        let _ = write!(items, "<li>{message}</li>");
    }
    format!("<ul class=\"differential-meta-notice\">{items}</ul>")
}

/// `PhabricatorDiffScopeEngine`: the line a gap's context belongs to.
struct ScopeEngine {
    /// The new file's lines, from line 1.
    texts: Vec<String>,
    depths: Vec<Option<i64>>,
}

impl ScopeEngine {
    /// Tabs count for two spaces.
    const TAB_WIDTH: i64 = 2;

    fn new(texts: Vec<String>) -> Self {
        let depths = texts
            .iter()
            .map(|text| {
                if text
                    .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
                    .is_empty()
                {
                    return None;
                }
                let mut count = 0;
                for byte in text.bytes() {
                    match byte {
                        b' ' => count += 1,
                        b'\t' => count += Self::TAB_WIDTH,
                        _ => break,
                    }
                }
                Some(count / Self::TAB_WIDTH)
            })
            .collect();
        Self { texts, depths }
    }

    /// `getScopeStart()`.
    fn scope_start(&self, line: i64) -> Option<i64> {
        let length = i64::try_from(self.texts.len()).unwrap_or(i64::MAX);
        let depth = |line: i64| -> Option<i64> {
            usize::try_from(line - 1)
                .ok()
                .and_then(|index| self.depths.get(index).copied().flatten())
        };
        let mut ii = line;
        let mut line_depth = None;
        while ii <= length {
            if let Some(found) = depth(ii) {
                line_depth = Some(found);
                break;
            }
            ii += 1;
        }
        let line_depth = line_depth?;
        let search_until = (ii - 1000).max(0);
        let mut ii = line - 1;
        while ii > search_until {
            let text = &self.texts[usize::try_from(ii - 1).ok()?];
            if let Some(found) = depth(ii)
                && found <= 2
                && found < line_depth
                && !text
                    .trim_start_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C'])
                    .starts_with(['{', '}'])
            {
                return Some(ii);
            }
            ii -= 1;
        }
        None
    }
}

/// `renderUndoTemplates()` of the side by side renderer.
fn undo_templates() -> Map<String, Value> {
    let undo = "<div class=\"differential-inline-undo\">Changes discarded. <a href=\"#\" \
                data-sigil=\"differential-inline-comment-undo\">Undo</a></div>";
    let row = |left: &str, right: &str| {
        format!(
            "<table><tr class=\"inline\" data-sigil=\"inline-row\"><td class=\"n\"></td><td \
             class=\"left\">{left}</td><td class=\"n\"></td><td class=\"copy\"></td><td \
             colspan=\"2\">{right}</td></tr></table>"
        )
    };
    let mut templates = Map::new();
    templates.insert("l".to_owned(), json!(row(undo, "")));
    templates.insert("r".to_owned(), json!(row("", undo)));
    templates
}

/// What the request renders with what the changeset and the cache hold.
pub(crate) struct Rendering<'a> {
    pub(crate) changeset: &'a Changeset,
    pub(crate) cache: &'a ParseCache,
    pub(crate) request: &'a Request,
    pub(crate) encoding: Option<&'a str>,
    /// `isGeneratedChangeset()` and `differential.generated-paths` are in
    /// the cache; the shields' other conditions are not.
    pub(crate) is_hidden: bool,
}

/// `calculateGapsAndMask()`.
fn gaps_and_mask(
    cache: &ParseCache,
    mask_force: &[i64],
    range_start: i64,
    range_len: i64,
    lines_context: i64,
) -> (Vec<(i64, i64)>, HashSet<i64>) {
    let mut base: HashSet<i64> = cache.visible.iter().copied().collect();
    base.extend(mask_force.iter().copied());
    base.insert(range_start + range_len);
    let mut gaps = Vec::new();
    let mut gap_start = 0;
    let mut in_gap = false;
    for ii in range_start..=range_start + range_len {
        if base.contains(&ii) {
            if in_gap {
                let gap_length = ii - gap_start;
                if gap_length <= lines_context {
                    for jj in gap_start..=gap_start + gap_length {
                        base.insert(jj);
                    }
                } else {
                    gaps.push((gap_start, gap_length));
                }
                in_gap = false;
            }
        } else if !in_gap {
            gap_start = ii;
            in_gap = true;
        }
    }
    gaps.reverse();
    (gaps, base)
}

/// The changeset's markup, as `render()` makes it.
fn render(rendering: &Rendering<'_>, meta: &mut Metadata) -> Result<String, PageError> {
    let Rendering {
        changeset,
        cache,
        request,
        encoding,
        ..
    } = rendering;
    let (range_start, range_len, mask_force) = &request.range;
    let is_top_level = range_start.is_none() && range_len.is_none();
    let rows = i64::try_from(cache.old.len().max(cache.new.len()))
        .map_err(|_| unsupported("huge changesets"))?;
    let mut renderer = TwoUp {
        changeset,
        cache,
        reference: &request.reference,
        encoding: *encoding,
        is_top_level,
        is_undershield: false,
        gaps: Vec::new(),
        mask: HashSet::new(),
    };
    if is_top_level {
        let shield = if cache.special("attr:generated") {
            Some((
                "This file contains generated code, which does not normally need to be \
                 reviewed."
                    .to_owned(),
                "default",
            ))
        } else if cache.special("attr:moveaway") {
            return Ok(renderer.render_changeset_table(""));
        } else if cache.special("attr:unchanged") {
            let force = if rows == 0 { "none" } else { "text" };
            let message = if changeset.change_type == ChangeType::Add {
                "This is an empty file."
            } else {
                "The contents of this file were not changed."
            };
            Some((message.to_owned(), force))
        } else if cache.special("attr:deleted") {
            Some(("This file was completely deleted.".to_owned(), "default"))
        } else if changeset.affected_line_count() > LARGE_CHANGE_LINES {
            Some((
                format!(
                    "This file has a very large number of changes ({} lines).",
                    number(changeset.affected_line_count())
                ),
                "default",
            ))
        } else {
            None
        };
        if let Some((message, force)) = shield {
            let shield = renderer.render_shield(meta, &message, force);
            return Ok(renderer.render_changeset_table(&shield));
        }
    }
    renderer.is_undershield = mask_force.is_empty() || is_top_level;
    let range_start = range_start.unwrap_or(0);
    let range_len = range_len.unwrap_or(rows).min(rows - range_start);
    let (gaps, mask) = gaps_and_mask(
        cache,
        mask_force,
        range_start,
        range_len,
        i64::try_from(phab_diff::view::DEFAULT_LINES_OF_CONTEXT).unwrap_or(8),
    );
    renderer.gaps = gaps;
    renderer.mask = mask;
    let html = renderer.render_text_change(meta, range_start, range_len, rows)?;
    Ok(renderer.render_changeset_table(&html))
}

/// The payload of `newChangesetResponse()`, with the coverage the
/// controller adds and the metadata of the answer.
pub(crate) fn response(rendering: &Rendering<'_>, block: i64) -> Result<Value, PageError> {
    let mut meta = Metadata::new(block);
    let changeset = render(rendering, &mut meta)?;
    let mut payload = Map::new();
    payload.insert("changeset".to_owned(), json!(changeset));
    payload.insert("undoTemplates".to_owned(), Value::Object(undo_templates()));
    payload.insert("rendererKey".to_owned(), json!("2up"));
    payload.insert("highlight".to_owned(), Value::Null);
    payload.insert("characterEncoding".to_owned(), Value::Null);
    payload.insert("requestDocumentEngineKey".to_owned(), Value::Null);
    payload.insert("responseDocumentEngineKey".to_owned(), Value::Null);
    payload.insert("availableDocumentEngineKeys".to_owned(), json!(["source"]));
    payload.insert("isHidden".to_owned(), json!(rendering.is_hidden));
    let digest = hex::encode(Md5::digest(rendering.changeset.filename.as_bytes()));
    payload.insert(
        "coverage".to_owned(),
        json!({ format!("differential-mcoverage-{digest}"): "<em>-</em>" }),
    );
    let mut answer = Map::new();
    answer.insert("error".to_owned(), Value::Null);
    answer.insert("payload".to_owned(), Value::Object(payload));
    if !meta.entries.is_empty() {
        answer.insert("javelin_metadata".to_owned(), Value::Array(meta.entries));
    }
    Ok(Value::Object(answer))
}

/// The text of the side, as the document engines get it
/// (`getRawDocumentEngineData()`).
fn document_data(side: &[(i64, Option<LineSpec>)]) -> String {
    side.iter()
        .filter_map(|(_, spec)| spec.as_ref())
        .filter(|spec| !spec.is_no_newline())
        .map(|spec| spec.text.as_str())
        .collect()
}

/// Whether a document engine which can diff (Jupyter, which takes any
/// JSON) may render the changeset: PHP lists it then, or renders with it.
pub(crate) fn may_use_document_engine(changeset: &Changeset, cache: &ParseCache) -> bool {
    let looks_like_json = |data: &str| {
        data.trim_start_matches([' ', '\t', '\n', '\r'])
            .starts_with(['{', '['])
    };
    changeset.filename.to_ascii_lowercase().ends_with(".ipynb")
        || looks_like_json(&document_data(&cache.old))
        || looks_like_json(&document_data(&cache.new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_parsed_like_php() {
        assert_eq!(parse_range(None), (None, None, vec![]));
        assert_eq!(parse_range(Some("10-30")), (Some(10), Some(30), vec![]));
        assert_eq!(
            parse_range(Some("10-30/12-3")),
            (Some(10), Some(30), vec![12, 13, 14])
        );
        assert_eq!(parse_range(Some("x")), (None, None, vec![]));
        assert_eq!(parse_range(Some("1-2/x")), (None, None, vec![]));
    }

    #[test]
    fn paths_split_like_php() {
        assert_eq!(basename("a/b/c.txt"), "c.txt");
        assert_eq!(dirname("/a/b/c.txt"), "/a/b");
        assert_eq!(dirname("/c.txt"), "/");
    }

    #[test]
    fn scopes_are_found_like_php() {
        let engine = ScopeEngine::new(
            ["int main() {\n", "  int x;\n", "\n", "  return x;\n", "}\n"]
                .map(str::to_owned)
                .to_vec(),
        );
        assert_eq!(engine.scope_start(4), Some(1));
        assert_eq!(engine.scope_start(1), None);
    }

    #[test]
    fn gaps_are_computed_like_php() {
        let cache = ParseCache {
            visible: (0..3).chain(40..43).collect(),
            ..ParseCache::default()
        };
        let (gaps, mask) = gaps_and_mask(&cache, &[], 0, 43, 8);
        assert_eq!(gaps, vec![(3, 37)]);
        assert!(mask.contains(&2) && !mask.contains(&3) && mask.contains(&40));
    }
}
