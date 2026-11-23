//! What `HarbormasterHTTPRequestBuildStepImplementation::execute()` does
//! before and around its request: the URI with the target's variables
//! merged in (`mergeVariables('vurisprintf', ...)`), the checks of
//! `PhabricatorEnv::requireSafeRemoteURIForFetch()` (protocol, domain, DNS,
//! `security.outbound-blacklist`), and the request itself, which goes
//! through an [`HttpClient`] so that what is sent and what is logged of the
//! answer can be reproduced.

use std::fmt::Write as _;
use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;

use phab_core::Config;
use serde_json::{Map, Value};

use super::{EngineError, unsupported};

/// The request a "Make HTTP Request" step sends, as `HTTPSFuture` is set
/// up: no redirects followed, a 60 second timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET`, `POST`, `PUT` or `DELETE`.
    pub method: String,
    /// The URI to fetch: for `http`, with the domain replaced by the
    /// address it resolved to.
    pub uri: String,
    /// The `Host` header, for `http`.
    pub host: Option<String>,
    /// The `Content-Type` header, if the step sets one.
    pub content_type: Option<String>,
    /// The body, if the step has one.
    pub body: Option<String>,
}

/// What `HTTPSFuture::resolve()` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status, or cURL's error number when the request failed
    /// (`HTTPFutureCURLResponseStatus`), which is always an error.
    pub status: u32,
    /// Whether `status` is cURL's error number.
    pub transport_error: bool,
    /// The headers, as `parseRawHTTPResponse()` lists them.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// `$status->isError()`: anything but a 2xx answer.
    pub fn is_error(&self) -> bool {
        self.transport_error || !(200..=299).contains(&self.status)
    }
}

/// The future [`HttpClient::send`] returns.
pub type HttpFuture<'a> = Pin<Box<dyn Future<Output = HttpResponse> + Send + 'a>>;

/// Sends the requests of HTTP steps. Without one, the targets which would
/// send a request are left to PHP.
pub trait HttpClient: Send + Sync {
    /// Sends `request` and returns the answer as PHP's `HTTPSFuture` reads
    /// it.
    fn send<'a>(&'a self, request: &'a HttpRequest) -> HttpFuture<'a>;
}

/// How the step goes once its URI is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
    /// `requireSafeRemoteURIForFetch()` threw: the step logs "HTTP Request:
    /// Invalid URI" and fails.
    InvalidUri,
    /// The request to send, and the URI the logs are named after.
    Send(HttpRequest, String),
}

/// `phutil_escape_uri()`: `rawurlencode()`, with `/` kept.
fn escape_uri(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            escaped.push(char::from(byte));
        } else {
            let _ = write!(escaped, "%{byte:02X}");
        }
    }
    escaped
}

/// A variable as `%s` of `vurisprintf()` prints it.
fn variable_text(value: &Value) -> Result<String, EngineError> {
    match value {
        Value::Null | Value::Bool(false) => Ok(String::new()),
        Value::String(text) => Ok(escape_uri(text)),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            Ok(escape_uri(&number.to_string()))
        }
        Value::Bool(true) => Ok("1".to_owned()),
        _ => Err(unsupported(
            "HTTP steps using variables which are not scalars",
        )),
    }
}

/// Whether `byte` is in `[a-z\./_-]`, the variable names `mergeVariables()`
/// knows.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || matches!(byte, b'.' | b'/' | b'_' | b'-')
}

/// `mergeVariables('vurisprintf', $pattern, $variables)`: each
/// `${name}` replaced by the escaped variable. A missing variable throws,
/// which the target worker logs with its stack trace: left to PHP.
pub(super) fn merge_variables(
    pattern: &str,
    variables: &Map<String, Value>,
) -> Result<String, EngineError> {
    let bytes = pattern.as_bytes();
    let mut merged = String::with_capacity(pattern.len());
    let mut copied = 0;
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'$' && bytes[index + 1] == b'{' {
            let start = index + 2;
            let mut end = start;
            while end < bytes.len() && is_name_byte(bytes[end]) {
                end += 1;
            }
            if end > start && end < bytes.len() && bytes[end] == b'}' {
                let name = &pattern[start..end];
                let value = variables
                    .get(name)
                    .ok_or_else(|| unsupported("HTTP steps using unknown variables"))?;
                merged.push_str(&pattern[copied..index]);
                merged.push_str(&variable_text(value)?);
                index = end + 1;
                copied = index;
                continue;
            }
        }
        index += 1;
    }
    merged.push_str(&pattern[copied..]);
    Ok(merged)
}

/// The parts of a URI `PhutilURI` parses the same way: only plain
/// `scheme://host[:port][/path][?query][#fragment]` URIs, whose host
/// `PhutilURI` accepts; anything else (user names, IPv6 literals,
/// whitespace, ambiguous URIs) is left to PHP.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed<'a> {
    protocol: &'a str,
    domain: &'a str,
    port: Option<&'a str>,
    rest: &'a str,
}

fn parse_uri(uri: &str) -> Result<Parsed<'_>, EngineError> {
    let odd = || unsupported("HTTP steps with unusual URIs");
    if uri
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(odd());
    }
    let (protocol, after) = uri.split_once("://").ok_or_else(odd)?;
    if protocol.is_empty() || !protocol.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(odd());
    }
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(authority_end);
    if authority.contains(['@', '[', ']']) {
        return Err(odd());
    }
    let (domain, port) = match authority.split_once(':') {
        Some((domain, port)) => {
            if port.is_empty()
                || port.len() > 5
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || port
                    .parse::<u32>()
                    .map_or(true, |port| port == 0 || port > 65535)
            {
                return Err(odd());
            }
            (domain, Some(port))
        }
        None => (authority, None),
    };
    // `^[a-zA-Z0-9]+[a-zA-Z0-9\.\-]*\z`.
    let valid_host = domain
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
    if !valid_host {
        return Err(odd());
    }
    // Rejected as ambiguous by `PhutilURI`.
    if rest.starts_with(['?', '#']) && rest.contains(':') {
        return Err(odd());
    }
    Ok(Parsed {
        protocol,
        domain,
        port,
        rest,
    })
}

/// An IPv4 block of `security.outbound-blacklist`.
fn parse_block(block: &str) -> Result<Option<(u32, u32)>, EngineError> {
    let (address, bits) = block.split_once('/').unwrap_or((block, "32"));
    if address.contains(':') {
        // IPv6 blocks never contain the IPv4 addresses `gethostbynamel()`
        // returns.
        return Ok(None);
    }
    let address: Ipv4Addr = address
        .parse()
        .map_err(|_| unsupported("unusual outbound blacklists"))?;
    let bits: u32 = bits
        .parse()
        .ok()
        .filter(|bits| *bits <= 32)
        .ok_or_else(|| unsupported("unusual outbound blacklists"))?;
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    Ok(Some((u32::from(address) & mask, mask)))
}

/// `PhutilCIDRList::newList($blacklist)->containsAddress($address)`.
fn is_blacklisted(config: &Config, address: Ipv4Addr) -> Result<bool, EngineError> {
    let blocks: Vec<String> = match config.value("security.outbound-blacklist") {
        None => DEFAULT_BLACKLIST
            .iter()
            .map(|block| (*block).to_owned())
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| unsupported("unusual outbound blacklists"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(unsupported("unusual outbound blacklists")),
    };
    let address = u32::from(address);
    for block in &blocks {
        if let Some((network, mask)) = parse_block(block)?
            && address & mask == network
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The IPv4 part of `security.outbound-blacklist`'s default.
const DEFAULT_BLACKLIST: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.88.99.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
    "255.255.255.255/32",
];

/// `gethostbynamel()`: the IPv4 addresses of `domain`, empty when it
/// doesn't resolve.
async fn resolve(domain: &str) -> Vec<Ipv4Addr> {
    if let Ok(address) = domain.parse::<Ipv4Addr>() {
        return vec![address];
    }
    match tokio::net::lookup_host((domain, 0)).await {
        Ok(addresses) => {
            let mut found = Vec::new();
            for address in addresses {
                if let std::net::IpAddr::V4(v4) = address.ip()
                    && !found.contains(&v4)
                {
                    found.push(v4);
                }
            }
            found
        }
        Err(_) => Vec::new(),
    }
}

/// A setting of the step (`$settings[$key]`): a string, or nothing.
fn setting<'a>(
    settings: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, EngineError> {
    match settings.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(unsupported(
            "HTTP steps with settings which are not strings",
        )),
    }
}

/// Plans `execute()` up to its request, for a target with these settings
/// (`details`) and variables (with `target.phid`).
pub(super) async fn plan(
    config: &Config,
    settings: &Map<String, Value>,
    variables: &Map<String, Value>,
) -> Result<Plan, EngineError> {
    let pattern =
        setting(settings, "uri")?.ok_or_else(|| unsupported("HTTP steps without a URI"))?;
    let uri = merge_variables(pattern, variables)?;
    let method = match setting(settings, "method")? {
        Some(method) if phab_core::php::is_truthy_str(method) => method.to_owned(),
        _ => "POST".to_owned(),
    };
    let parsed = parse_uri(&uri)?;
    if parsed.protocol != "http" && parsed.protocol != "https" {
        return Ok(Plan::InvalidUri);
    }
    let addresses = resolve(parsed.domain).await;
    let Some(first) = addresses.first() else {
        return Ok(Plan::InvalidUri);
    };
    for address in &addresses {
        if is_blacklisted(config, *address)? {
            return Ok(Plan::InvalidUri);
        }
    }
    if settings
        .get("credential")
        .is_some_and(phab_core::php::is_truthy)
    {
        return Err(unsupported("HTTP steps with credentials"));
    }
    // `$settings['content_type']` and `$settings['http_body']` warn when
    // the step doesn't have them.
    if !settings.contains_key("content_type") || !settings.contains_key("http_body") {
        return Err(unsupported(
            "HTTP steps without a content type or body setting",
        ));
    }
    let content_type = setting(settings, "content_type")?
        .filter(|text| phab_core::php::is_truthy_str(text))
        .map(str::to_owned);
    let body = match setting(settings, "http_body")? {
        Some(body) if phab_core::php::is_truthy_str(body) => {
            Some(merge_variables(body, variables)?)
        }
        _ => None,
    };
    let (fetch_uri, host) = if parsed.protocol == "http" {
        let host = match parsed.port {
            Some(port) => format!("{}:{port}", parsed.domain),
            None => parsed.domain.to_owned(),
        };
        let port = parsed
            .port
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        (format!("http://{first}{port}{}", parsed.rest), Some(host))
    } else {
        (uri.clone(), None)
    };
    if uri.len() > super::log::SOURCE_LIMIT {
        return Err(unsupported(
            "HTTP steps whose URI is longer than a log source",
        ));
    }
    Ok(Plan::Send(
        HttpRequest {
            method,
            uri: fetch_uri,
            host,
            content_type,
            body,
        },
        uri,
    ))
}

/// The `http.head` log of an answer (`logHTTPResponse()`).
pub(super) fn head_log(response: &HttpResponse) -> String {
    let mut lines = vec![format!("HTTP {}", response.status)];
    for (name, value) in &response.headers {
        lines.push(format!("{name}: {value}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    #[test]
    fn merges_variables_like_vurisprintf() {
        let variables = map(json!({
            "build.id": "5",
            "buildable.diff": 3,
            "initiator.phid": null,
            "target.phid": "PHID-HMBT-a b",
        }));
        assert_eq!(
            merge_variables(
                "https://ci.test/run?b=${build.id}&d=${buildable.diff}%",
                &variables
            )
            .unwrap(),
            "https://ci.test/run?b=5&d=3%"
        );
        assert_eq!(
            merge_variables("x/${target.phid}/${initiator.phid}${", &variables).unwrap(),
            "x/PHID-HMBT-a%20b/${"
        );
        assert!(merge_variables("${nope}", &variables).is_err());
        assert_eq!(merge_variables("${Build}", &variables).unwrap(), "${Build}");
    }

    #[test]
    fn parses_plain_uris_only() {
        let parsed = parse_uri("http://ci.example.test:8080/run?x=1").unwrap();
        assert_eq!(parsed.protocol, "http");
        assert_eq!(parsed.domain, "ci.example.test");
        assert_eq!(parsed.port, Some("8080"));
        assert_eq!(parsed.rest, "/run?x=1");
        assert!(parse_uri("http://user@ci.test/").is_err());
        assert!(parse_uri("http://-ci.test/").is_err());
        assert!(parse_uri(" http://ci.test/").is_err());
        assert!(parse_uri("ci.test/run").is_err());
        assert_eq!(parse_uri("ftp://ci.test/").unwrap().protocol, "ftp");
    }

    #[test]
    fn checks_the_blacklist() {
        let config = Config::default();
        assert!(is_blacklisted(&config, Ipv4Addr::LOCALHOST).unwrap());
        assert!(is_blacklisted(&config, Ipv4Addr::new(10, 1, 2, 3)).unwrap());
        assert!(!is_blacklisted(&config, Ipv4Addr::new(8, 8, 8, 8)).unwrap());
    }

    #[tokio::test]
    async fn plans_invalid_uris() {
        let config = Config::default();
        let variables = Map::new();
        let settings = map(json!({"uri": "https://localhost/run", "method": "GET"}));
        assert_eq!(
            plan(&config, &settings, &variables).await.unwrap(),
            Plan::InvalidUri
        );
        let settings = map(json!({"uri": "gopher://ci.test/"}));
        assert_eq!(
            plan(&config, &settings, &variables).await.unwrap(),
            Plan::InvalidUri
        );
        let settings = map(json!({"uri": "http://name.invalid/"}));
        assert_eq!(
            plan(&config, &settings, &variables).await.unwrap(),
            Plan::InvalidUri
        );
    }

    #[test]
    fn logs_the_head() {
        let response = HttpResponse {
            status: 200,
            transport_error: false,
            headers: vec![("Server".to_owned(), "x".to_owned())],
            body: Vec::new(),
        };
        assert_eq!(head_log(&response), "HTTP 200\nServer: x");
        assert!(!response.is_error());
    }
}
