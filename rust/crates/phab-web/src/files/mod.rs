//! Files pages (`PhabricatorFilesApplication`): a file's page (`/F<id>`,
//! `/file/info/<phid>/`), its data (`/file/data/...`, see [`data`]) and
//! the builtin queries of the file list.
//!
//! Files are loaded as `file.search` loads them, as the viewer, so a page
//! shows what PHP would; files visible through objects Rust doesn't load
//! go to PHP. Editing, deleting, uploading and transforms link to PHP.

mod data;

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_app_files::storage::{ENGINE_BLOB, ENGINE_CHUNKS, FORMAT_RAW, Storage};
use phab_app_files::{self as files, File};
use phab_conduit::Method as _;
use phab_core::Phid;
use serde_json::{Value, json};

pub(crate) use data::{data, download};

use crate::error::PageError;
use crate::harbormaster::{Action, Handle, Scope, handle, load_handles, method_error};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, time};

/// `PhabricatorFilesApplication`.
const APPLICATION: &str = "PhabricatorFilesApplication";

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: u32 = 100;

/// Text files longer than this are not shown on their page.
const TEXT_LIMIT: u64 = 1024 * 1024;

/// `F123`, as a monogram.
pub(crate) fn parse_monogram(monogram: &str) -> Option<i64> {
    let digits = monogram.strip_prefix('F')?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// How the page shows the file itself.
pub(crate) enum Content {
    Image { uri: String, alt: String },
    Audio { uri: String },
    Video { uri: String },
    Text(String),
    None,
}

/// A property of the "Details" tab.
pub(crate) struct Property {
    name: &'static str,
    value: String,
}

#[derive(Template)]
#[template(path = "file.html")]
pub(crate) struct FilePage {
    shell: Shell,
    monogram: String,
    name: String,
    temporary: bool,
    partial: bool,
    content: Content,
    properties: Vec<Property>,
    /// The objects the file is attached to: link HTML and type.
    attached: Vec<String>,
    chunks: Option<(usize, usize)>,
    actions: Vec<Action>,
    author_html: Option<String>,
    created: String,
    size: String,
    dimensions: Option<String>,
}

/// `GET /file/info/<phid>/`: a redirect to the file's page.
pub(crate) async fn info<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
) -> Result<Response, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let file = load_visible(state.as_ref(), &viewer, &Phid::new(phid)).await?;
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/{}", file.monogram())) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    Ok(response)
}

/// `GET /file/view/<id>/`.
pub(crate) async fn view<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<FilePage>, PageError> {
    show(state, viewer, id, uri).await
}

/// The file the viewer can see with this PHID, unless it was deleted.
async fn load_visible<S: WebState>(
    state: &S,
    viewer: &WebUser,
    phid: &Phid,
) -> Result<File, PageError> {
    let database = state.database();
    let candidates = files::load_by_phids(database, std::slice::from_ref(phid)).await?;
    let details = files::details(database, std::slice::from_ref(phid)).await?;
    if details.get(phid).is_none_or(|details| details.is_deleted) {
        return Err(PageError::NotFound);
    }
    let scope = Scope::new(state, viewer);
    files::search::visible_files(&scope.request(), candidates)
        .await
        .map_err(method_error)?
        .into_iter()
        .next()
        // PHP's policy exception page.
        .ok_or(PageError::Fallback("a file the viewer can't see"))
}

/// `PhabricatorFileStorageFormat::getStorageFormatName()`.
fn format_name(engine: &str, format: &str) -> String {
    if engine == ENGINE_CHUNKS {
        return "Chunks".to_owned();
    }
    match format {
        FORMAT_RAW => "Raw Data".to_owned(),
        "aes-256-cbc" => "Encrypted (AES-256-CBC)".to_owned(),
        "rot13" => "Encoded (ROT13)".to_owned(),
        other => format!("Unknown (\"{other}\")"),
    }
}

/// `GET /F<id>` (`PhabricatorFileViewController`).
pub(crate) async fn show<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    id: i64,
    uri: Uri,
) -> Result<Page<FilePage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let database = state.database();
    let config = state.config();
    let phid = files::load_by_ids(database, &[id])
        .await?
        .into_iter()
        .next()
        .ok_or(PageError::NotFound)?
        .phid;
    let file = load_visible(state.as_ref(), &viewer, &phid).await?;
    let details = files::details(database, std::slice::from_ref(&phid))
        .await?
        .remove(&phid)
        .ok_or(PageError::NotFound)?;
    if files::has_transactions(database, &phid).await? {
        return Err(PageError::Fallback("a file with a timeline"));
    }
    let storage = Storage::load(database, phid.as_str())
        .await
        .map_err(method_error)?
        .ok_or(PageError::NotFound)?;
    if storage.engine != ENGINE_BLOB && storage.engine != ENGINE_CHUNKS {
        return Err(PageError::Fallback("files outside MySQL"));
    }

    let scope = Scope::new(state.as_ref(), &viewer);
    let attached_phids = files::attached_objects(database, std::slice::from_ref(&file))
        .await?
        .remove(&phid)
        .unwrap_or_default();
    let mut handle_phids = attached_phids.clone();
    handle_phids.extend(file.author_phid.clone());
    let handles: HashMap<Phid, Handle> =
        load_handles(state.as_ref(), &scope, &handle_phids).await?;

    let content = content(state.as_ref(), &file, &storage).await?;
    let can_edit = file.author_phid.as_ref() == Some(&viewer.user.phid);
    let actions = actions(&file, config, can_edit);

    let properties = properties(&file, config, &details, &storage);
    let chunks = if storage.engine == ENGINE_CHUNKS {
        let all = files::chunk_completion(database, &storage.handle).await?;
        Some((all.len(), all.iter().filter(|complete| **complete).count()))
    } else {
        None
    };

    let dimensions = file.image_size(config).map(|(width, height)| {
        format!(
            "{}px \u{d7} {}px",
            files::group_thousands(u64::from(width)),
            files::group_thousands(u64::from(height))
        )
    });
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(FilePage {
        shell: Shell::for_user(config, &viewer, &file.name, current),
        monogram: file.monogram(),
        name: file.name.clone(),
        temporary: details.ttl.is_some(),
        partial: file.is_partial,
        content,
        properties,
        attached: attached_phids
            .iter()
            .map(|phid| handle(&handles, phid).link_html())
            .collect(),
        chunks,
        actions,
        author_html: file
            .author_phid
            .as_ref()
            .map(|author| handle(&handles, author).short_link_html()),
        created: time::format_utc(details.date_created),
        size: files::format_bytes(file.byte_size),
        dimensions,
    }))
}

/// The curtain's actions (`buildCurtainView()`).
fn actions(file: &File, config: &phab_core::Config, can_edit: bool) -> Vec<Action> {
    let can_download = !file.is_partial;
    let mut actions = Vec::new();
    if file.is_viewable_in_browser(config) {
        actions.push(Action {
            name: "View File",
            icon: "fa-file-o",
            uri: file.view_uri(config),
            enabled: can_download,
        });
    } else {
        actions.push(Action {
            name: "Download File",
            icon: "fa-download",
            uri: file.download_uri(config),
            enabled: can_download,
        });
    }
    actions.push(Action {
        name: "Edit File",
        icon: "fa-pencil",
        uri: format!("/file/edit/{}/", file.id),
        enabled: can_edit,
    });
    actions.push(Action {
        name: "Delete File",
        icon: "fa-times",
        uri: format!("/file/delete/{}/", file.id),
        enabled: can_edit,
    });
    actions.push(Action {
        name: "View Transforms",
        icon: "fa-crop",
        uri: format!("/file/transforms/{}/", file.id),
        enabled: true,
    });
    actions
}

/// The "Details" tab (`buildPropertyViews()`).
fn properties(
    file: &File,
    config: &phab_core::Config,
    details: &files::Details,
    storage: &Storage,
) -> Vec<Property> {
    let mut properties = vec![Property {
        name: "Mime Type",
        value: file.mime_type.clone(),
    }];
    if let Some(ttl) = details.ttl.filter(|ttl| *ttl != 0) {
        properties.push(Property {
            name: "Expires",
            value: time::format_utc(ttl),
        });
    }
    let mut types = Vec::new();
    if file.is_viewable_image(config) {
        types.push("Image".to_owned());
    }
    if file.is_video(config) {
        types.push("Video".to_owned());
    }
    if file.is_audio(config) {
        types.push("Audio".to_owned());
    }
    if file.can_cdn(config) {
        types.push("Can CDN".to_owned());
    }
    if let Some(builtin) = file.builtin_name() {
        types.push(format!("Builtin (\"{builtin}\")"));
    }
    if file.metadata_truthy("profile") {
        types.push("Profile".to_owned());
    }
    if !types.is_empty() {
        properties.push(Property {
            name: "Attributes",
            value: types.join(", "),
        });
    }
    properties.push(Property {
        name: "Storage Engine",
        value: storage.engine.clone(),
    });
    properties.push(Property {
        name: "Storage Format",
        value: format_name(&storage.engine, &storage.format),
    });
    properties.push(Property {
        name: "Storage Handle",
        value: storage.handle.clone(),
    });
    if let Some(custom) = file
        .custom_alt_text()
        .and_then(Value::as_str)
        .filter(|alt| !alt.is_empty())
    {
        properties.push(Property {
            name: "Custom Alt Text",
            value: custom.to_owned(),
        });
    }
    let default_alt = file.default_alt_text(config);
    if !default_alt.is_empty() {
        properties.push(Property {
            name: "Default Alt Text",
            value: default_alt,
        });
    }
    properties
}

/// What the page shows of the file's contents.
async fn content<S: WebState>(
    state: &S,
    file: &File,
    storage: &Storage,
) -> Result<Content, PageError> {
    let config = state.config();
    if file.is_partial || !file.is_viewable_in_browser(config) {
        return Ok(Content::None);
    }
    if file.is_viewable_image(config) {
        return Ok(Content::Image {
            uri: file.view_uri(config),
            alt: file.alt_text(config),
        });
    }
    if file.is_video(config) {
        return Ok(Content::Video {
            uri: file.view_uri(config),
        });
    }
    if file.is_audio(config) {
        return Ok(Content::Audio {
            uri: file.view_uri(config),
        });
    }
    let bare = file.mime_type.split(';').next().unwrap_or("").trim();
    if bare.starts_with("text/") && file.byte_size <= TEXT_LIMIT {
        let data = storage
            .load_data(state.database())
            .await
            .map_err(method_error)?;
        if let Ok(text) = String::from_utf8(data) {
            return Ok(Content::Text(text));
        }
    }
    Ok(Content::None)
}

/// A file in the list.
pub(crate) struct FileItem {
    id: i64,
    phid: String,
    name: String,
    uploaded_html: String,
    size: String,
    temporary: bool,
    partial: bool,
}

#[derive(Template)]
#[template(path = "files.html")]
pub(crate) struct FileList {
    shell: Shell,
    title: &'static str,
    key: &'static str,
    items: Vec<FileItem>,
    next: Option<String>,
}

/// `GET /file/`: the viewer's default query, "Authored".
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<FileList>, PageError> {
    show_list(state, viewer, "authored", query, uri).await
}

/// `GET /file/query/<key>/`: the builtin queries.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<FileList>, PageError> {
    let key = match key.as_str() {
        "authored" => "authored",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved file query")),
    };
    show_list(state, viewer, key, query, uri).await
}

/// The `after` cursor of a list's query string; any other parameter is a
/// query PHP builds.
fn cursor(query: Option<&str>) -> Result<Option<String>, PageError> {
    let mut after = None;
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match key.as_ref() {
            "after" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                after = Some(value.into_owned());
            }
            _ => return Err(PageError::Fallback("a search query")),
        }
    }
    Ok(after)
}

async fn show_list<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    key: &'static str,
    query: Option<String>,
    uri: Uri,
) -> Result<Page<FileList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let after = cursor(query.as_deref())?;
    // The default query is the first builtin one unless the viewer saved
    // or reordered theirs.
    if phab_app_harbormaster::web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        "PhabricatorFileSearchEngine",
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    let scope = Scope::new(state.as_ref(), &viewer);
    let mut params = serde_json::Map::new();
    if key == "authored" {
        params.insert(
            "constraints".to_owned(),
            json!({"authorPHIDs": [viewer.user.phid.as_str()], "explicit": true}),
        );
    }
    params.insert("limit".to_owned(), json!(PAGE_SIZE));
    if let Some(after) = &after {
        params.insert("after".to_owned(), json!(after));
    }
    let request = scope.request_with(params);
    let result = files::search::FileSearch
        .execute(&request)
        .await
        .map_err(method_error)?;
    let results = result
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let next = result
        .pointer("/cursor/after")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let ids: Vec<i64> = results
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_i64))
        .collect();
    let mut loaded: HashMap<i64, File> = files::load_by_ids(state.database(), &ids)
        .await?
        .into_iter()
        .map(|file| (file.id, file))
        .collect();
    let rows: Vec<File> = ids.iter().filter_map(|id| loaded.remove(id)).collect();
    let phids: Vec<Phid> = rows.iter().map(|file| file.phid.clone()).collect();
    let details = files::details(state.database(), &phids).await?;
    let authors: Vec<Phid> = rows
        .iter()
        .filter_map(|file| file.author_phid.clone())
        .collect();
    let handles = load_handles(state.as_ref(), &scope, &authors).await?;
    let items = rows
        .iter()
        .map(|file| {
            let details = details.get(&file.phid);
            let date = details.map_or_else(String::new, |details| {
                time::format_utc(details.date_created)
            });
            let uploaded_html = match &file.author_phid {
                Some(author) => format!(
                    "Uploaded by {} on {}",
                    handle(&handles, author).link_html(),
                    diff_html::escape(&date)
                ),
                None => format!("Uploaded on {}", diff_html::escape(&date)),
            };
            FileItem {
                id: file.id,
                phid: file.phid.as_str().to_owned(),
                name: file.name.clone(),
                uploaded_html,
                size: files::format_bytes(file.byte_size),
                temporary: details.is_some_and(|details| details.ttl.is_some()),
                partial: file.is_partial,
            }
        })
        .collect();
    let title = if key == "authored" { "Authored" } else { "All" };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(FileList {
        shell: Shell::for_user(state.config(), &viewer, title, current),
        title,
        key,
        items,
        next,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monograms_are_file_ids() {
        assert_eq!(parse_monogram("F12"), Some(12));
        assert_eq!(parse_monogram("F012"), None);
        assert_eq!(parse_monogram("F"), None);
        assert_eq!(parse_monogram("D12"), None);
    }

    #[test]
    fn storage_formats_are_named_like_php() {
        assert_eq!(format_name("blob", "raw"), "Raw Data");
        assert_eq!(format_name("chunks", "raw"), "Chunks");
        assert_eq!(format_name("blob", "x"), "Unknown (\"x\")");
    }
}
