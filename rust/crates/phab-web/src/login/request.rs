//! The parts of `AphrontRequest` the login pages use: request data, the
//! cookies PHP reads and sets, and the host checks around them.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use phab_auth::user_log::RequestInfo;
use phab_core::Config;

use crate::time;

/// `setCookie()`: cookies which "do not expire for a long time".
const LONG_COOKIE_LIFETIME: i64 = 60 * 60 * 24 * 365 * 5;

/// `rawurlencode()`, which `setcookie()` applies to values.
pub(crate) use phab_auth::oauth::rawurlencode;

/// `rawurldecode()`, which PHP applies to cookie values: `+` stays.
fn rawurldecode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |offset: usize| {
            bytes
                .get(index + offset)
                .and_then(|byte| char::from(*byte).to_digit(16))
        };
        match (bytes[index], hex(1), hex(2)) {
            (b'%', Some(high), Some(low)) => {
                decoded.push(u8::try_from(high * 16 + low).unwrap_or_default());
                index += 3;
            }
            (byte, _, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The request cookies as `$_COOKIE` holds them: decoded, the first of
/// duplicate names winning.
fn parse_cookies(headers: &HeaderMap) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    for pair in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
    {
        if let Some((name, value)) = pair.trim().split_once('=') {
            cookies
                .entry(name.to_owned())
                .or_insert_with(|| rawurldecode(value));
        }
    }
    cookies
}

/// Form fields or query parameters, as PHP parses them.
fn parse_pairs(data: &[u8]) -> Vec<(String, String)> {
    form_urlencoded::parse(data)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// The last value of `name` in `pairs`.
fn last_value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// A request to one of the login pages. Not `Debug`: it holds the
/// session cookie and the posted password.
pub(crate) struct LoginRequest {
    pub method: Method,
    pub uri: Uri,
    headers: HeaderMap,
    get: Vec<(String, String)>,
    post: Vec<(String, String)>,
    cookies: HashMap<String, String>,
    set_cookies: Vec<String>,
}

impl LoginRequest {
    pub fn new(method: Method, uri: Uri, headers: HeaderMap, body: &Bytes) -> Self {
        let get = parse_pairs(uri.query().unwrap_or_default().as_bytes());
        let is_form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
        let post = if method == Method::POST && is_form {
            parse_pairs(body)
        } else {
            Vec::new()
        };
        let cookies = parse_cookies(&headers);
        Self {
            method,
            uri,
            headers,
            get,
            post,
            cookies,
            set_cookies: Vec::new(),
        }
    }

    /// `getStr()`: request data is `$_POST + $_GET`, and the last of
    /// duplicate names wins.
    pub fn str(&self, name: &str) -> Option<&str> {
        last_value(&self.post, name).or_else(|| last_value(&self.get, name))
    }

    /// `getStr()` of a parameter which must not be empty.
    pub fn nonempty_str(&self, name: &str) -> Option<&str> {
        self.str(name).filter(|value| !value.is_empty())
    }

    /// `getBool()`: set and truthy, `"false"` being false.
    pub fn bool(&self, name: &str) -> bool {
        self.str(name)
            .is_some_and(|value| value != "false" && phab_core::php::is_truthy_str(value))
    }

    /// `getArr()` of a parameter posted as `name[key]=value`: the keys and
    /// values, from the form if it has any, from the query otherwise.
    pub fn array(&self, name: &str) -> Vec<(String, String)> {
        let entries = |pairs: &[(String, String)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .filter_map(|(key, value)| {
                    let inner = key
                        .strip_prefix(name)?
                        .strip_prefix('[')?
                        .strip_suffix(']')?;
                    (!inner.contains(['[', ']'])).then(|| (inner.to_owned(), value.clone()))
                })
                .collect()
        };
        let posted = entries(&self.post);
        if posted.is_empty() {
            entries(&self.get)
        } else {
            posted
        }
    }

    /// `getExists()`.
    pub fn exists(&self, name: &str) -> bool {
        self.str(name).is_some()
    }

    /// `isHTTPPost()`.
    pub fn is_post(&self) -> bool {
        self.method == Method::POST
    }

    /// `getCookie()`: a cookie PHP deleted reads as missing.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .get(name)
            .map(String::as_str)
            .filter(|value| *value != "deleted")
    }

    /// `getCookie()` of a cookie which must not be empty.
    pub fn nonempty_cookie(&self, name: &str) -> Option<&str> {
        self.cookie(name).filter(|value| !value.is_empty())
    }

    /// A header as text.
    pub fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// `isHTTPS()`: nginx says how the client reached it.
    fn is_https(&self) -> bool {
        self.headers
            .get("x-forwarded-proto")
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"https"))
    }

    /// `getHost()`: the domain of the `Host` header, parsed like a URI so
    /// that `real.example:x@evil.example` gives `evil.example`.
    fn host(&self) -> Option<String> {
        let host = self.header(header::HOST)?;
        let parsed = reqwest::Url::parse(&format!("http://{host}")).ok()?;
        parsed.host_str().map(str::to_owned)
    }

    /// `PhabricatorEnv::getRemoteAddress()`: PHP sees nginx's peer, which
    /// nginx appends last to `X-Forwarded-For`.
    pub fn remote_address(&self) -> String {
        self.header(header::HeaderName::from_static("x-forwarded-for"))
            .and_then(|value| value.rsplit(',').next())
            .map(|address| address.trim().to_owned())
            .unwrap_or_default()
    }

    /// What the activity log records about this request.
    pub fn info(&self, hostname: &str) -> RequestInfo {
        RequestInfo {
            remote_address: self.remote_address(),
            user_agent: self.header(header::USER_AGENT).map(str::to_owned),
            hostname: hostname.to_owned(),
        }
    }

    /// `getCookieDomainURI()`: the domain cookies are set on and whether
    /// they are secure, or `None` when PHP refuses to set cookies.
    pub fn cookie_domain(&self, config: &Config) -> Option<(String, bool)> {
        if config.truthy("security.require-https") && !self.is_https() {
            return None;
        }
        let host = self.host()?;
        let Some(base_uri) = config.nonempty_string("phabricator.base-uri") else {
            return Some((host, false));
        };
        let alternates = config
            .value("phabricator.allowed-uris")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str);
        std::iter::once(base_uri)
            .chain(alternates)
            .filter_map(|uri| reqwest::Url::parse(uri).ok())
            .find(|uri| uri.host_str() == Some(host.as_str()))
            .map(|uri| (host, uri.scheme() == "https"))
    }

    fn push_cookie(
        &mut self,
        config: &Config,
        name: &str,
        value: &str,
        expires: Option<i64>,
        now: i64,
    ) {
        let Some((domain, secure)) = self.cookie_domain(config) else {
            // Handlers check `cookie_domain()` before writing anything.
            return;
        };
        let mut line = if value.is_empty() {
            // PHP's `setcookie()` deletes a cookie set to "".
            format!(
                "{name}=deleted; expires={}; Max-Age=0",
                time::format_cookie_expiry(1)
            )
        } else {
            match expires {
                Some(expires) => format!(
                    "{name}={}; expires={}; Max-Age={}",
                    rawurlencode(value),
                    time::format_cookie_expiry(expires),
                    (expires - now).max(0)
                ),
                None => format!("{name}={}", rawurlencode(value)),
            }
        };
        line.push_str("; path=/; domain=");
        line.push_str(&domain);
        if secure {
            line.push_str("; secure");
        }
        line.push_str("; HttpOnly");
        self.set_cookies.push(line);
        if value.is_empty() {
            self.cookies.remove(name);
        } else {
            self.cookies.insert(name.to_owned(), value.to_owned());
        }
    }

    /// `setCookie()`.
    pub fn set_cookie(&mut self, config: &Config, name: &str, value: &str, now: i64) {
        self.push_cookie(config, name, value, Some(now + LONG_COOKIE_LIFETIME), now);
    }

    /// `setTemporaryCookie()`: a cookie for the browser session.
    pub fn set_temporary_cookie(&mut self, config: &Config, name: &str, value: &str, now: i64) {
        self.push_cookie(config, name, value, None, now);
    }

    /// `clearCookie()`.
    pub fn clear_cookie(&mut self, config: &Config, name: &str, now: i64) {
        self.push_cookie(config, name, "", Some(now - 60 * 60 * 24 * 30), now);
    }

    /// Adds the cookies set so far to `response`.
    pub fn finish(self, mut response: Response) -> Response {
        let headers = response.headers_mut();
        for line in self.set_cookies {
            if let Ok(value) = HeaderValue::from_str(&line) {
                headers.append(header::SET_COOKIE, value);
            }
        }
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }

    /// An `AphrontRedirectResponse` to a local path, made absolute with
    /// the base URI, with the cookies set so far.
    pub fn redirect(self, config: &Config, path: &str) -> Response {
        let location = config.uri(path).unwrap_or_else(|_| path.to_owned());
        self.redirect_external(&location)
    }

    /// A redirect to `location` as is.
    pub fn redirect_external(self, location: &str) -> Response {
        let mut response = StatusCode::FOUND.into_response();
        if let Ok(value) = HeaderValue::from_str(location) {
            response.headers_mut().insert(header::LOCATION, value);
        }
        self.finish(response)
    }

    /// The path and query of the request without `__path__`
    /// (`getRequestURI()`), with `cleared` replaced or removed.
    pub fn uri_with_cleared(&self, cleared: bool) -> String {
        let mut pairs: Vec<String> = self
            .get
            .iter()
            .filter(|(key, _)| key != "__path__" && key != "cleared")
            .map(|(key, value)| format!("{}={}", rawurlencode(key), rawurlencode(value)))
            .collect();
        if cleared {
            pairs.push("cleared=1".to_owned());
        }
        let path = self.uri.path();
        if pairs.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{}", pairs.join("&"))
        }
    }
}

/// `PhabricatorEnv::isValidLocalURIForLink()`.
pub(crate) fn is_valid_local_uri(uri: &str) -> bool {
    if uri.is_empty() || uri.contains('\\') {
        return false;
    }
    // PCRE's `\s`: space, \t, \n, \v, \f, \r.
    if uri
        .bytes()
        .any(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        return false;
    }
    let mut bytes = uri.bytes();
    bytes.next() == Some(b'/') && bytes.next() != Some(b'/')
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn request(headers: &[(&str, &str)], uri: &str) -> LoginRequest {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        LoginRequest::new(Method::GET, uri.parse().unwrap(), map, &Bytes::new())
    }

    fn config(values: &serde_json::Value) -> Config {
        let mut config = Config::default();
        config.push_layer("test", values.as_object().unwrap().clone());
        config
    }

    #[test]
    fn cookies_decode_like_php() {
        let request = request(
            &[("cookie", "a=A%2Fb+c%20d; b=deleted; c=x=y; a=second")],
            "/",
        );
        assert_eq!(request.cookie("a"), Some("A/b+c d"));
        assert_eq!(request.cookie("b"), None);
        assert_eq!(request.cookie("c"), Some("x=y"));
    }

    // Expected headers come from PHP 8.4's setcookie().
    #[test]
    fn set_cookies_match_php() {
        let config = config(&json!({"phabricator.base-uri": "https://phab.example.com/"}));
        let mut request = request(&[("host", "phab.example.com")], "/");
        let now = 1_953_039_015 - LONG_COOKIE_LIFETIME;
        request.set_cookie(&config, "phsid", "A/abc+ d,x", now);
        request.clear_cookie(&config, "phcid", now);
        request.set_temporary_cookie(&config, "phtmp", "abc", now);
        assert_eq!(request.cookie("phsid"), Some("A/abc+ d,x"));
        assert_eq!(
            request.set_cookies,
            [
                "phsid=A%2Fabc%2B%20d%2Cx; expires=Fri, 21 Nov 2031 14:50:15 GMT; \
                 Max-Age=157680000; path=/; domain=phab.example.com; secure; HttpOnly",
                "phcid=deleted; expires=Thu, 01 Jan 1970 00:00:01 GMT; Max-Age=0; path=/; \
                 domain=phab.example.com; secure; HttpOnly",
                "phtmp=abc; path=/; domain=phab.example.com; secure; HttpOnly",
            ]
        );
    }

    #[rstest]
    #[case("phab.example.com", None, Some(("phab.example.com", true)))]
    #[case("alt.example.com", None, Some(("alt.example.com", false)))]
    #[case("evil.example", None, None)]
    #[case("phab.example.com:x@evil.example", None, None)]
    #[case("phab.example.com:8080", None, Some(("phab.example.com", true)))]
    #[case("phab.example.com", Some("http"), None)]
    fn cookie_domains(
        #[case] host: &str,
        #[case] proto: Option<&str>,
        #[case] expected: Option<(&str, bool)>,
    ) {
        let mut config = config(&json!({
            "phabricator.base-uri": "https://phab.example.com/",
            "phabricator.allowed-uris": ["http://alt.example.com/"],
        }));
        let mut headers = vec![("host", host)];
        if let Some(proto) = proto {
            headers.push(("x-forwarded-proto", proto));
            config.push_layer(
                "https",
                json!({"security.require-https": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        assert_eq!(
            request(&headers, "/").cookie_domain(&config),
            expected.map(|(domain, secure)| (domain.to_owned(), secure))
        );
    }

    #[rstest]
    #[case("/D1", true)]
    #[case("/", true)]
    #[case("//evil.example", false)]
    #[case("/\\evil.example", false)]
    #[case("https://evil.example", false)]
    #[case("/a b", false)]
    #[case("", false)]
    fn local_uris(#[case] uri: &str, #[case] expected: bool) {
        assert_eq!(is_valid_local_uri(uri), expected);
    }

    #[test]
    fn remote_addresses_are_the_last_hop() {
        let request = request(&[("x-forwarded-for", "6.6.6.6, 10.0.0.1")], "/");
        assert_eq!(request.remote_address(), "10.0.0.1");
    }

    #[test]
    fn cleared_uris() {
        let request = request(&[], "/auth/start/?next=%2FD1&cleared=1&__path__=x");
        assert_eq!(request.uri_with_cleared(false), "/auth/start/?next=%2FD1");
        assert_eq!(
            request.uri_with_cleared(true),
            "/auth/start/?next=%2FD1&cleared=1"
        );
    }
}
