//! Files (`PhabricatorFile`): what pages show of them, and who may see
//! them (`PhabricatorFileQuery`).
//!
//! The file contents are still served by PHP (`/file/data/...`).

pub mod info;
pub mod search;

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::hash::BuildHasher;

use phab_core::{Config, Phid, php};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{
    AutomaticCapability, Capability, ObjectKind, POLICY_NOONE, POLICY_PUBLIC, POLICY_USER,
    PolicySubject, Viewer,
};
use serde_json::Value;

/// Registers the Files methods.
pub fn register(registry: &mut phab_conduit::Registry) {
    registry.register(info::FileInfo);
    registry.register(search::FileSearch);
}

/// `PhabricatorFileAttachment::MODE_ATTACH`.
const MODE_ATTACH: &str = "attach";

/// `PhabricatorFileThumbnailTransform::TRANSFORM_PREVIEW`.
pub const TRANSFORM_PREVIEW: &str = "preview";

/// The width of the preview thumbnail.
const PREVIEW_WIDTH: f64 = 220.0;

const COLUMNS: &str =
    "id, phid, name, mimeType, byteSize, authorPHID, secretKey, metadata, viewPolicy, isPartial";

/// A `file` row.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    /// The row ID.
    pub id: i64,
    /// The file PHID.
    pub phid: Phid,
    /// The name, like `screenshot.png`.
    pub name: String,
    /// The MIME type.
    pub mime_type: String,
    /// The size in bytes.
    pub byte_size: u64,
    /// Who uploaded it.
    pub author_phid: Option<Phid>,
    /// The key in its URIs.
    pub secret_key: String,
    /// The view policy.
    pub view_policy: String,
    /// `isPartial`: still being uploaded.
    pub is_partial: bool,
    metadata: serde_json::Map<String, Value>,
}

impl File {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        let metadata = match rows::json(row, "metadata") {
            Ok(Value::Object(metadata)) => metadata,
            _ => serde_json::Map::new(),
        };
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::opt_text(row, "name")?.unwrap_or_default(),
            mime_type: rows::opt_text(row, "mimeType")?.unwrap_or_default(),
            byte_size: rows::uint(row, "byteSize")?,
            author_phid: rows::opt_phid(row, "authorPHID")?,
            secret_key: rows::opt_text(row, "secretKey")?.unwrap_or_default(),
            view_policy: rows::text(row, "viewPolicy")?,
            is_partial: rows::flag(row, "isPartial")?,
            metadata,
        })
    }

    /// `getMonogram()`.
    pub fn monogram(&self) -> String {
        format!("F{}", self.id)
    }

    /// Whether a metadata key is truthy, like `profile`.
    pub fn metadata_truthy(&self, key: &str) -> bool {
        self.metadata.get(key).is_some_and(php::is_truthy)
    }

    /// `isBuiltin()`.
    pub fn is_builtin(&self) -> bool {
        self.metadata
            .get("builtin")
            .is_some_and(|value| !value.is_null())
    }

    fn mime_map_has(config: &Config, key: &str, defaults: &[&str], mime_type: &str) -> bool {
        match config.object(key) {
            Some(map) => map.get(mime_type).is_some_and(php::is_truthy),
            None => defaults.contains(&mime_type),
        }
    }

    /// `isViewableInBrowser()`: the type, without its parameters, is in
    /// `files.viewable-mime-types`.
    pub fn is_viewable_in_browser(&self, config: &Config) -> bool {
        let mime_type = self.mime_type.split(';').next().unwrap_or("").trim();
        Self::mime_map_has(
            config,
            "files.viewable-mime-types",
            VIEWABLE_DEFAULT,
            mime_type,
        )
    }

    /// `isViewableImage()`.
    pub fn is_viewable_image(&self, config: &Config) -> bool {
        self.is_viewable_in_browser(config)
            && Self::mime_map_has(
                config,
                "files.image-mime-types",
                IMAGE_DEFAULT,
                &self.mime_type,
            )
    }

    /// `isAudio()`.
    pub fn is_audio(&self, config: &Config) -> bool {
        self.is_viewable_in_browser(config)
            && Self::mime_map_has(
                config,
                "files.audio-mime-types",
                AUDIO_DEFAULT,
                &self.mime_type,
            )
    }

    /// `isVideo()`.
    pub fn is_video(&self, config: &Config) -> bool {
        self.is_viewable_in_browser(config)
            && Self::mime_map_has(
                config,
                "files.video-mime-types",
                VIDEO_DEFAULT,
                &self.mime_type,
            )
    }

    fn metadata_int(&self, key: &str) -> Option<u32> {
        match self.metadata.get(key)? {
            Value::Number(number) => number.as_u64().and_then(|value| u32::try_from(value).ok()),
            Value::String(text) => text.parse().ok(),
            _ => None,
        }
        .filter(|value| *value > 0)
    }

    /// `getImageWidth()` and `getImageHeight()` of a viewable image.
    pub fn image_size(&self, config: &Config) -> Option<(u32, u32)> {
        if !self.is_viewable_image(config) {
            return None;
        }
        Some((self.metadata_int("width")?, self.metadata_int("height")?))
    }

    /// `getAltText()`: the custom text, or the name with the size.
    pub fn alt_text(&self, config: &Config) -> String {
        match self.metadata.get("alt").and_then(Value::as_str) {
            Some(alt) if !alt.is_empty() => alt.to_owned(),
            _ => self.default_alt_text(config),
        }
    }

    /// `getCustomAltText()`: the `alt` metadata, as stored.
    pub fn custom_alt_text(&self) -> Option<&Value> {
        self.metadata.get("alt")
    }

    /// `getDefaultAltText()`: "name (H×W px, size)". PHP swaps the width
    /// and the height.
    pub fn default_alt_text(&self, config: &Config) -> String {
        let mut parts = Vec::new();
        if !self.name.is_empty() {
            parts.push(self.name.clone());
        }
        let mut stats = Vec::new();
        if let Some((width, height)) = self.image_size(config) {
            stats.push(format!(
                "{}\u{d7}{} px",
                group_thousands(u64::from(height)),
                group_thousands(u64::from(width))
            ));
        }
        if self.byte_size > 0 {
            stats.push(format_bytes(self.byte_size));
        }
        if !stats.is_empty() {
            parts.push(format!("({})", stats.join(", ")));
        }
        parts.join(" ")
    }

    fn cdn_path(&self, config: &Config, kind: &str) -> String {
        let mut parts = vec!["file".to_owned(), kind.to_owned()];
        if let Some(instance) = config.nonempty_string("cluster.instance") {
            parts.push(format!("@{instance}"));
        }
        parts.push(self.secret_key.clone());
        parts.push(self.phid.as_str().to_owned());
        parts.push(escape_uri(&normalize_file_name(&self.name)));
        format!("/{}", parts.join("/"))
    }

    /// `getCDNURI()`: partial files are on the install itself.
    fn cdn_uri(&self, config: &Config, kind: &str) -> String {
        let path = self.cdn_path(config, kind);
        let uri = if self.is_partial {
            config.uri(&path)
        } else {
            config.cdn_uri(&path)
        };
        uri.unwrap_or(path)
    }

    /// `getViewURI()`.
    pub fn view_uri(&self, config: &Config) -> String {
        self.cdn_uri(config, "data")
    }

    /// `getDownloadURI()`.
    pub fn download_uri(&self, config: &Config) -> String {
        self.cdn_uri(config, "download")
    }

    /// `getBestURI()`: the file itself if browsers show it, its page
    /// otherwise.
    pub fn best_uri(&self, config: &Config) -> String {
        if self.is_viewable_in_browser(config) {
            self.view_uri(config)
        } else {
            format!("/{}", self.monogram())
        }
    }

    /// `getURIForTransform()`: where a transform not made yet is made.
    pub fn transform_uri(&self, config: &Config, transform: &str) -> String {
        let mut parts = vec!["file".to_owned(), "xform".to_owned()];
        if let Some(instance) = config.nonempty_string("cluster.instance") {
            parts.push(format!("@{instance}"));
        }
        parts.push(transform.to_owned());
        parts.push(self.phid.as_str().to_owned());
        parts.push(self.secret_key.clone());
        let path = format!("/{}/", parts.join("/"));
        config.cdn_uri(&path).unwrap_or(path)
    }

    /// `getTransformedDimensions()` of the preview transform: 220 pixels
    /// on the longer side, never thinner than a quarter of that. PHP
    /// computes floats and prints them as they are.
    pub fn preview_size(&self, config: &Config) -> Option<(String, String)> {
        let (width, height) = self.image_size(config)?;
        let (width, height) = (f64::from(width), f64::from(height));
        let (x, y) = if width < height {
            (
                (PREVIEW_WIDTH / 4.0).max(PREVIEW_WIDTH * (width / height)),
                PREVIEW_WIDTH,
            )
        } else {
            (
                PREVIEW_WIDTH,
                (PREVIEW_WIDTH / 4.0).max(PREVIEW_WIDTH * (height / width)),
            )
        };
        Some((php_float(x), php_float(y)))
    }

    /// The file as a policy object (`getPolicy()` and
    /// `hasAutomaticCapability()`); `attached_visible` says whether the
    /// viewer can see an object the file is attached to, or the file it
    /// is a thumbnail of.
    pub fn subject(
        &self,
        config: &Config,
        viewer: &Viewer,
        attached_visible: bool,
    ) -> PolicySubject {
        let most_open = if config.allow_public() {
            POLICY_PUBLIC
        } else {
            POLICY_USER
        };
        let view = if self.is_builtin() || self.metadata_truthy("profile") {
            most_open.to_owned()
        } else {
            self.view_policy.clone()
        };
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, view)
            .with_policy(Capability::EDIT, POLICY_NOONE);
        if let Some(author) = &self.author_phid {
            subject.automatic.push(AutomaticCapability {
                principal: author.clone(),
                capabilities: None,
            });
        }
        if attached_visible && let Some(phid) = &viewer.phid {
            subject.automatic.push(AutomaticCapability {
                principal: phid.clone(),
                capabilities: Some(vec![Capability::VIEW]),
            });
        }
        subject
    }
}

/// `PhutilNumber`: digits grouped by thousands.
fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `phutil_format_bytes()`.
pub fn format_bytes(bytes: u64) -> String {
    const LABELS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut whole = bytes;
    let mut label = 0;
    while whole >= 1024 && label + 1 < LABELS.len() {
        whole /= 1024;
        label += 1;
    }
    format!("{whole} {}", LABELS[label])
}

/// PHP's `(string)` of a float: integers without a fraction.
fn php_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// `files.viewable-mime-types` by default.
const VIEWABLE_DEFAULT: &[&str] = &[
    "image/jpeg",
    "image/jpg",
    "image/png",
    "image/gif",
    "text/plain",
    "text/x-diff",
    "image/x-ico",
    "image/x-icon",
    "image/vnd.microsoft.icon",
];
/// `files.image-mime-types` by default.
const IMAGE_DEFAULT: &[&str] = &[
    "image/jpeg",
    "image/jpg",
    "image/png",
    "image/gif",
    "image/x-ico",
    "image/x-icon",
    "image/vnd.microsoft.icon",
];
/// `files.audio-mime-types` by default.
const AUDIO_DEFAULT: &[&str] = &[
    "audio/x-wav",
    "audio/mpeg",
    "audio/ogg",
    "video/mp4",
    "video/ogg",
    "video/quicktime",
    "application/ogg",
];
/// `files.video-mime-types` by default.
const VIDEO_DEFAULT: &[&str] = &[
    "video/mp4",
    "video/ogg",
    "video/webm",
    "video/quicktime",
    "application/ogg",
];

/// `PhabricatorFile::normalizeFileName()`.
pub fn normalize_file_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for character in name.chars() {
        // Runs of unsafe characters and of underscores become one "_".
        let replaced = character <= '\x19' || "#%&+!~'$\"/=\\?<> _".contains(character);
        if !replaced {
            normalized.push(character);
        } else if !normalized.ends_with('_') {
            normalized.push('_');
        }
    }
    let trimmed = normalized.trim_matches('_');
    match trimmed {
        "." => "dot".to_owned(),
        ".." => "dotdot".to_owned(),
        "" => "file".to_owned(),
        _ => trimmed.to_owned(),
    }
}

/// `phutil_escape_uri()`: `rawurlencode()`, keeping `/`.
fn escape_uri(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(char::from(byte));
        } else {
            write!(output, "%{byte:02X}").expect("writing to a String");
        }
    }
    output
}

/// The files with these IDs; unknown IDs are skipped.
pub async fn load_by_ids(database: &Database, ids: &[i64]) -> Result<Vec<File>, DbError> {
    load_where(
        database,
        "id",
        ids.iter().map(ToString::to_string).collect(),
    )
    .await
}

/// The files with these PHIDs; unknown PHIDs are skipped.
pub async fn load_by_phids(database: &Database, phids: &[Phid]) -> Result<Vec<File>, DbError> {
    load_where(
        database,
        "phid",
        phids.iter().map(|phid| phid.as_str().to_owned()).collect(),
    )
    .await
}

async fn load_where(
    database: &Database,
    column: &str,
    values: Vec<String>,
) -> Result<Vec<File>, DbError> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE {column} IN ({})",
        database.table("file", "file"),
        phab_db::placeholders(values.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), values)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(File::from_row)
        .collect()
}

/// The objects each file is attached to (`newAttachmentsMap()`). Builtin
/// files are visible anyway and are left out.
pub async fn attached_objects(
    database: &Database,
    files: &[File],
) -> Result<HashMap<Phid, Vec<Phid>>, DbError> {
    let phids: Vec<&str> = files
        .iter()
        .filter(|file| !file.is_builtin())
        .map(|file| file.phid.as_str())
        .collect();
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT filePHID, objectPHID FROM {} WHERE filePHID IN ({}) AND attachmentMode = ?",
        database.table("file", "file_attachment"),
        phab_db::placeholders(phids.len()),
    );
    let mut map: HashMap<Phid, Vec<Phid>> = HashMap::new();
    for row in phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
        .bind(MODE_ATTACH)
        .fetch_all(database.pool())
        .await?
    {
        map.entry(Phid::new(rows::text(&row, "filePHID")?))
            .or_default()
            .push(Phid::new(rows::text(&row, "objectPHID")?));
    }
    Ok(map)
}

/// The file each of these is a transform of (`PhabricatorTransformedFile`),
/// by transformed PHID.
pub async fn originals(
    database: &Database,
    files: &[File],
) -> Result<HashMap<Phid, Phid>, DbError> {
    let phids: Vec<&str> = files
        .iter()
        .filter(|file| !file.is_builtin())
        .map(|file| file.phid.as_str())
        .collect();
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT originalPHID, transformedPHID FROM {} WHERE transformedPHID IN ({})",
        database.table("file", "file_transformedfile"),
        phab_db::placeholders(phids.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "transformedPHID")?),
                Phid::new(rows::text(row, "originalPHID")?),
            ))
        })
        .collect()
}

/// The `transform` thumbnails made of these files, by original PHID
/// (`needTransforms()`).
pub async fn transforms(
    database: &Database,
    files: &[File],
    transform: &str,
) -> Result<HashMap<Phid, File>, DbError> {
    if files.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT originalPHID, transformedPHID FROM {} WHERE originalPHID IN ({}) AND transform = ?",
        database.table("file", "file_transformedfile"),
        phab_db::placeholders(files.len()),
    );
    let pairs: Vec<(Phid, Phid)> = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        files.iter().map(|file| file.phid.as_str()),
    )
    .bind(transform)
    .fetch_all(database.pool())
    .await?
    .iter()
    .map(|row| {
        Ok((
            Phid::new(rows::text(row, "originalPHID")?),
            Phid::new(rows::text(row, "transformedPHID")?),
        ))
    })
    .collect::<Result<_, DbError>>()?;
    let transformed: Vec<Phid> = pairs.iter().map(|(_, phid)| phid.clone()).collect();
    let mut by_phid: HashMap<Phid, File> = load_by_phids(database, &transformed)
        .await?
        .into_iter()
        .map(|file| (file.phid.clone(), file))
        .collect();
    Ok(pairs
        .into_iter()
        .filter_map(|(original, transformed)| Some((original, by_phid.remove(&transformed)?)))
        .collect())
}

/// Why visibility could not be decided.
#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    /// Loading the policy facts failed.
    #[error(transparent)]
    Loader(#[from] phab_policy::loader::LoaderError),
    /// The policy filter refused the check.
    #[error(transparent)]
    Policy(#[from] phab_policy::PolicyError),
}

/// The files `viewer` can see. `attached_visible` holds the files which
/// are attached to an object the viewer can see, or are thumbnails of a
/// file the viewer can see (`PhabricatorFileQuery::loadPage()`).
pub async fn visible<H: BuildHasher>(
    database: &Database,
    config: &Config,
    viewer: &Viewer,
    files: Vec<File>,
    attached_visible: &HashSet<Phid, H>,
    now: i64,
) -> Result<Vec<File>, VisibilityError> {
    let subjects: Vec<PolicySubject> = files
        .iter()
        .map(|file| file.subject(config, viewer, attached_visible.contains(&file.phid)))
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts =
        phab_policy::loader::load_facts(database, config, viewer, &refs, &NoExtendedObjects, now)
            .await?;
    let passed = phab_policy::filter(viewer, &refs, &[Capability::VIEW], &facts)?;
    Ok(files
        .into_iter()
        .zip(passed)
        .filter_map(|(file, visible)| visible.then_some(file))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_normalized_like_php() {
        assert_eq!(normalize_file_name("a b#c.png"), "a_b_c.png");
        assert_eq!(normalize_file_name("__x__"), "x");
        assert_eq!(normalize_file_name("?"), "file");
        assert_eq!(normalize_file_name(".."), "dotdot");
    }

    #[test]
    fn sizes_print_like_php() {
        assert_eq!(group_thousands(1_234_567), "1,234,567");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(format_bytes(1536), "1 KB");
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn floats_print_like_php() {
        assert_eq!(php_float(220.0), "220");
        assert_eq!(php_float(220.0 * (2.0 / 3.0)), "146.66666666666666");
    }
}
