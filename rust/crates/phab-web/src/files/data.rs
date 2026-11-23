//! `/file/data/...` and `/file/download/...`
//! (`PhabricatorFileDataController`): a file's bytes, with the headers
//! PHP's `AphrontFileResponse` sends.
//!
//! The secret key in the URI is the only check, as in PHP: the file is
//! loaded as the omnipotent viewer. Rust answers the requests which get
//! the data; PHP keeps the others (421): unknown or deleted files, wrong
//! keys and partial uploads (PHP's dialogs), the redirect to the
//! alternate file domain, the confirmation dialog of downloads on the
//! main domain, `POST` requests, PDFs shown in the browser (their CSP),
//! odd `Range` headers, ranges of chunked files, and data Rust can't
//! read (storage engines other than MySQL, encrypted data).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use phab_app_files::File;
use phab_app_files::storage::{ENGINE_CHUNKS, Storage};

use crate::error::fallback_response;
use crate::php_headers::{self, Options, X_DOWNLOAD_OPTIONS};
use crate::state::WebState;

/// `setCacheDurationInSeconds(60 * 60 * 24 * 30)`.
const CACHE_DURATION: i64 = 60 * 60 * 24 * 30;

/// MIME types `shouldCompressFileDataResponse()` treats as compressed.
const COMPRESSED_TYPES: &[&str] = &[
    "application/x-gzip",
    "application/x-compress",
    "application/x-compressed",
    "application/x-zip-compressed",
    "application/zip",
];

/// The parts of the URI the controller reads.
#[derive(Debug, PartialEq, Eq)]
struct DataUri<'a> {
    key: &'a str,
    phid: &'a str,
}

/// The route's pattern, `(?:@(?P<instance>[^/]+)/)?(?P<key>[^/]+)/
/// (?P<phid>[^/]+)/(?:(?P<token>[^/]+)/)?.*`, after `/file/{kind}/`.
fn parse_uri(rest: &str) -> Option<DataUri<'_>> {
    // The segments which a slash ends.
    let mut segments: Vec<&str> = rest.split('/').collect();
    segments.pop();
    let complete = |index: usize| segments.get(index).copied().filter(|part| !part.is_empty());
    if segments
        .first()
        .is_some_and(|first| first.len() > 1 && first.starts_with('@'))
        && let (Some(key), Some(phid)) = (complete(1), complete(2))
    {
        return Some(DataUri { key, phid });
    }
    Some(DataUri {
        key: complete(0)?,
        phid: complete(1)?,
    })
}

/// A `Range` header as `AphrontFileResponse::parseHTTPRange()` reads it:
/// the first byte and the last one, if given. `Err` for a range PHP
/// would answer with headers which don't match its body.
fn parse_range(range: &str, length: u64) -> Result<Option<(u64, Option<u64>)>, ()> {
    let Some(spec) = range.strip_prefix("bytes=") else {
        return Ok(None);
    };
    let Some((begin, end)) = spec.split_once('-') else {
        return Ok(None);
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(begin) || !(end.is_empty() || digits(end)) {
        return Ok(None);
    }
    let begin: u64 = begin.parse().map_err(|_| ())?;
    let end: Option<u64> = if end.is_empty() {
        None
    } else {
        Some(end.parse().map_err(|_| ())?)
    };
    if begin >= length || end.is_some_and(|end| end < begin || end >= length) {
        return Err(());
    }
    Ok(Some((begin, end)))
}

/// Whether the file shown in the browser is compressed
/// (`shouldCompressFileDataResponse()`).
fn should_compress(file: &File, config: &phab_core::Config) -> bool {
    !(file.is_viewable_image(config)
        || file.is_audio(config)
        || file.is_video(config)
        || COMPRESSED_TYPES.contains(&file.mime_type.as_str()))
}

/// `addcslashes($filename, '"\\')`.
fn quote_file_name(name: &str) -> String {
    let name = if name.is_empty() { "untitled" } else { name };
    let mut quoted = String::with_capacity(name.len());
    for character in name.chars() {
        if matches!(character, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

/// `GET /file/data/...`.
pub(crate) async fn data<S: WebState>(
    state: State<Arc<S>>,
    method: Method,
    headers: HeaderMap,
    Path(rest): Path<String>,
) -> Response {
    serve(&*state.0, &method, &headers, false, &rest)
        .await
        .unwrap_or_else(fallback_response)
}

/// `GET /file/download/...`.
pub(crate) async fn download<S: WebState>(
    state: State<Arc<S>>,
    method: Method,
    headers: HeaderMap,
    Path(rest): Path<String>,
) -> Response {
    serve(&*state.0, &method, &headers, true, &rest)
        .await
        .unwrap_or_else(fallback_response)
}

/// Where the request is, as the controller decides it.
#[derive(Debug, PartialEq, Eq)]
enum Domain {
    /// No alternate file domain.
    Only,
    /// The main domain of an install with an alternate file domain: PHP
    /// redirects there.
    Main,
    /// The alternate file domain.
    Alternate,
}

fn request_domain(config: &phab_core::Config, request: &HeaderMap) -> Domain {
    let Some(alternate) = config.nonempty_string("security.alternate-file-domain") else {
        return Domain::Only;
    };
    let alternate = php_headers::domain(alternate);
    let main = config.base_uri().map_or("", php_headers::domain);
    if main == alternate {
        return Domain::Only;
    }
    let host = request
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map_or("", php_headers::domain);
    if host == alternate {
        Domain::Alternate
    } else {
        Domain::Main
    }
}

// The steps of the controller, in its order.
#[allow(clippy::too_many_lines)]
async fn serve<S: WebState>(
    state: &S,
    method: &Method,
    request: &HeaderMap,
    is_download_request: bool,
    rest: &str,
) -> Result<Response, &'static str> {
    if method != Method::GET && method != Method::HEAD {
        return Err("a file request which is not a GET");
    }
    let uri = parse_uri(rest).ok_or("a file URI PHP doesn't route")?;
    let config = state.config();
    let database = state.database();
    let domain = request_domain(config, request);
    let file = phab_app_files::load_undeleted(database, uri.phid)
        .await
        .map_err(|_| "a file which could not be loaded")?
        .ok_or("an unknown file")?;
    if file.secret_key != uri.key {
        return Err("a file URI with a wrong key");
    }
    if file.is_partial {
        return Err("a partial upload");
    }
    if domain == Domain::Main {
        return Err("a redirect to the alternate file domain");
    }

    let range = match request
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|range| !range.is_empty())
    {
        Some(range) => parse_range(range, file.byte_size).map_err(|()| "an unusual Range")?,
        None => None,
    };

    let viewable = file.viewable_mime_type(config);
    let is_download = is_download_request || viewable.is_none();
    let mime_type = if is_download {
        // The confirmation dialog of PHP's main domain is for logged-in
        // viewers; requests without a session download directly.
        let is_lfs = request
            .get("x-phabricator-request-type")
            .is_some_and(|value| value.as_bytes() == b"git-lfs");
        let is_public = crate::viewer::cookie(request, phab_auth::session::SESSION_COOKIE)
            .is_none_or(str::is_empty);
        if !(domain == Domain::Alternate || is_lfs || is_public) {
            return Err("the download dialog");
        }
        file.mime_type.clone()
    } else {
        if file.mime_type.split(';').next().map(str::trim) == Some("application/pdf") {
            return Err("a PDF shown in the browser");
        }
        viewable.unwrap_or_default()
    };

    let storage = Storage::load(database, file.phid.as_str())
        .await
        .map_err(|_| "file storage which could not be loaded")?
        .ok_or("a file without storage")?;
    if range.is_some() && storage.engine == ENGINE_CHUNKS {
        return Err("a range of a chunked file");
    }
    let mut data = storage
        .load_data(database)
        .await
        .map_err(|_| "file data Rust can't read")?;
    let length = file.byte_size;

    let mut response = Response::new(Body::empty());
    let headers = response.headers_mut();
    php_headers::base_headers(headers, config, request);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime_type).map_err(|_| "a MIME type which is not a header")?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let content_length = match range {
        Some((begin, end)) => {
            let last = end.unwrap_or(length - 1);
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {begin}-{last}/{length}"))
                    .map_err(|_| "a range which is not a header")?,
            );
            let start = usize::try_from(begin).map_err(|_| "a range too large")?;
            let stop = usize::try_from(last + 1).map_err(|_| "a range too large")?;
            data = data
                .get(start..stop.min(data.len()))
                .unwrap_or_default()
                .to_vec();
            last - begin + 1
        }
        None => length,
    };
    let compressed = should_compress(&file, config);
    if !compressed {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(content_length));
    }
    if is_download {
        headers.insert(X_DOWNLOAD_OPTIONS, HeaderValue::from_static("noopen"));
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_bytes(
                format!("attachment; filename=\"{}\"", quote_file_name(&file.name)).as_bytes(),
            )
            .map_err(|_| "a file name which is not a header")?,
        );
    }
    php_headers::cache_headers(
        headers,
        &Options {
            cache: Some((CACHE_DURATION, file.can_cdn(config))),
        },
        state.now(),
    );
    if compressed {
        data = php_headers::compress_output(headers, request, data);
    }
    if range.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    *response.body_mut() = Body::from(data);
    Ok(response.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_parsed_like_the_route() {
        assert_eq!(
            parse_uri("key/PHID-FILE-1/name.png"),
            Some(DataUri {
                key: "key",
                phid: "PHID-FILE-1"
            })
        );
        assert_eq!(
            parse_uri("@instance/key/PHID-FILE-1/name.png"),
            Some(DataUri {
                key: "key",
                phid: "PHID-FILE-1"
            })
        );
        // Without a third segment, "@x" is the key.
        assert_eq!(
            parse_uri("@x/PHID-FILE-1/"),
            Some(DataUri {
                key: "@x",
                phid: "PHID-FILE-1"
            })
        );
        assert_eq!(parse_uri("key/PHID-FILE-1"), None);
        assert_eq!(parse_uri("key//x/"), None);
    }

    #[test]
    fn ranges_are_parsed_like_php() {
        assert_eq!(parse_range("bytes=0-1", 10), Ok(Some((0, Some(1)))));
        assert_eq!(parse_range("bytes=5-", 10), Ok(Some((5, None))));
        assert_eq!(parse_range("bytes=-5", 10), Ok(None));
        assert_eq!(parse_range("items=0-1", 10), Ok(None));
        assert_eq!(parse_range("bytes=0-1,3-4", 10), Ok(None));
        assert_eq!(parse_range("bytes=10-", 10), Err(()));
        assert_eq!(parse_range("bytes=4-2", 10), Err(()));
    }

    #[test]
    fn file_names_are_quoted_like_addcslashes() {
        assert_eq!(quote_file_name(r#"a "b" \c.txt"#), r#"a \"b\" \\c.txt"#);
        assert_eq!(quote_file_name(""), "untitled");
    }
}
