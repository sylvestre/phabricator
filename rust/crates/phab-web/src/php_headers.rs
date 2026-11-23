//! The headers PHP's own responses carry (`AphrontResponse::getHeaders()`
//! and `getCacheHeaders()`), for the answers Rust gives in PHP's place
//! to PHP's pages and scripts: file data, typeahead results. Rust's own
//! pages use [`crate::html_response()`] instead.

use std::io::Write as _;

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use phab_core::Config;

/// `X-Download-Options`, which `http` doesn't name.
pub(crate) const X_DOWNLOAD_OPTIONS: HeaderName = HeaderName::from_static("x-download-options");

/// The parts of a response which change its headers.
#[derive(Debug, Default)]
pub(crate) struct Options {
    /// `setCacheDurationInSeconds()`, with `setCanCDN()`; `None` is not
    /// cacheable.
    pub(crate) cache: Option<(i64, bool)>,
}

/// Whether the request reached nginx over HTTPS (`AphrontRequest::isHTTPS()`
/// reads the `HTTPS` parameter PHP-FPM gets, which is nginx's `$https`; the
/// service gets it as `X-Forwarded-Proto`).
pub(crate) fn is_https(request: &HeaderMap) -> bool {
    request
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
}

/// The `scheme://authority` of a URI, as `newContentSecurityPolicySource()`
/// keeps it (no path, query or fragment).
pub(crate) fn origin(uri: &str) -> &str {
    let Some(scheme_end) = uri.find("://") else {
        return uri.split(['/', '?', '#']).next().unwrap_or(uri);
    };
    let start = scheme_end + 3;
    let end = uri[start..]
        .find(['/', '?', '#'])
        .map_or(uri.len(), |offset| start + offset);
    &uri[..end]
}

/// `PhutilURI::getDomain()` of a URI: its host, without credentials or
/// port.
pub(crate) fn domain(uri: &str) -> &str {
    let authority = origin(uri);
    let authority = authority
        .find("://")
        .map_or(authority, |end| &authority[end + 3..]);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if host.starts_with('[')
        && let Some(end) = host.find(']')
    {
        return &host[..=end];
    }
    host.split(':').next().unwrap_or(host)
}

/// `newContentSecurityPolicyHeader()` of a response which can't be framed,
/// with no source added by the page; `None` when PHP has no base URI.
pub(crate) fn content_security_policy(config: &Config) -> Option<String> {
    let base_uri = config.base_uri().ok()?;
    let cdn = config.nonempty_string("security.alternate-file-domain");
    let source = origin(cdn.unwrap_or(base_uri));
    // PHP throws on an unsafe source, and sends no policy.
    if source.contains([' ', ';', '\'']) {
        return None;
    }
    let default = if cdn.is_some() {
        source.to_owned()
    } else {
        format!("'self' {source}")
    };
    Some(format!(
        "default-src {default}; img-src {default} data:; style-src {default} 'unsafe-inline'; \
         script-src {default}; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; \
         object-src 'none'; form-action 'self'; base-uri 'none'"
    ))
}

/// `AphrontResponse::getHeaders()` of a response which can't be framed:
/// `X-Frame-Options`, HSTS over HTTPS, the CSP and `Referrer-Policy`.
pub(crate) fn base_headers(headers: &mut HeaderMap, config: &Config, request: &HeaderMap) {
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("Deny"));
    if is_https(request) {
        let duration = if config.truthy("security.strict-transport-security") {
            365 * 24 * 60 * 60
        } else {
            0
        };
        if let Ok(value) =
            HeaderValue::from_str(&format!("max-age={duration}; includeSubdomains; preload"))
        {
            headers.insert(header::STRICT_TRANSPORT_SECURITY, value);
        }
    }
    if let Some(value) =
        content_security_policy(config).and_then(|csp| HeaderValue::from_str(&csp).ok())
    {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

/// `AphrontResponse::getCacheHeaders()`, at `now` (PHP's `time()`).
pub(crate) fn cache_headers(headers: &mut HeaderMap, options: &Options, now: i64) {
    if let Some((duration, can_cdn)) = options.cache {
        let visibility = if can_cdn { "public" } else { "private" };
        if let Ok(value) = HeaderValue::from_str(&format!("max-age={duration}, {visibility}")) {
            headers.insert(header::CACHE_CONTROL, value);
        }
        if let Ok(value) = HeaderValue::from_str(&crate::time::format_cookie_expiry(now + duration))
        {
            headers.insert(header::EXPIRES, value);
        }
    } else {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::EXPIRES,
            HeaderValue::from_static("Sat, 01 Jan 2000 00:00:00 GMT"),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

/// `zlib.output_compression`: the body compressed with the encoding the
/// client accepts, gzip first, and the headers saying so.
pub(crate) fn compress_output(
    headers: &mut HeaderMap,
    request: &HeaderMap,
    body: Vec<u8>,
) -> Vec<u8> {
    let accepted = request
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let (encoding, compressed) = if accepted.contains("gzip") {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        (
            "gzip",
            encoder.write_all(&body).and_then(|()| encoder.finish()),
        )
    } else if accepted.contains("deflate") {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
        (
            "deflate",
            encoder.write_all(&body).and_then(|()| encoder.finish()),
        )
    } else {
        return body;
    };
    match compressed {
        Ok(compressed) => {
            headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
            headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
            compressed
        }
        Err(error) => {
            tracing::error!(%error, "compressing a response failed");
            body
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(values: serde_json::Value) -> Config {
        let mut config = Config::default();
        if let serde_json::Value::Object(map) = values {
            config.push_layer("test", map);
        }
        config
    }

    #[test]
    fn domains_drop_ports_and_credentials() {
        assert_eq!(domain("https://cdn.test:8443/x?y"), "cdn.test");
        assert_eq!(domain("http://user:pw@phab.test/"), "phab.test");
        assert_eq!(domain("phab.test:80"), "phab.test");
        assert_eq!(origin("https://cdn.test/some/path?x=1"), "https://cdn.test");
    }

    // Expected values from AphrontResponse::newContentSecurityPolicyHeader().
    #[test]
    fn the_policy_names_the_base_uri_or_the_file_domain() {
        assert_eq!(
            content_security_policy(&config(
                json!({"phabricator.base-uri": "http://phab.test/"})
            ))
            .unwrap(),
            "default-src 'self' http://phab.test; img-src 'self' http://phab.test data:; \
             style-src 'self' http://phab.test 'unsafe-inline'; script-src 'self' \
             http://phab.test; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; \
             object-src 'none'; form-action 'self'; base-uri 'none'"
        );
        assert_eq!(
            content_security_policy(&config(json!({
                "phabricator.base-uri": "http://phab.test/",
                "security.alternate-file-domain": "https://cdn.test/x",
            })))
            .unwrap(),
            "default-src https://cdn.test; img-src https://cdn.test data:; style-src \
             https://cdn.test 'unsafe-inline'; script-src https://cdn.test; connect-src 'self'; \
             frame-src 'self'; frame-ancestors 'none'; object-src 'none'; form-action 'self'; \
             base-uri 'none'"
        );
    }
}
