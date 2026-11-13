//! The part of `PhutilURI` Remarkup needs: whether a URI parses, its
//! protocol and whether it has a domain. It follows PHP's `parse_url()`
//! (`php_url_parse_ex2()`) for the scheme and host, which decide whether a
//! link is rendered.

use crate::preg::{self, Pattern};

static AMBIGUOUS: Pattern = Pattern::new(r"^[^/:]*://[^/]*[#?].*:");
static QUERY_ONLY: Pattern = Pattern::new(r"^([^/:]*://[^/]*)(\?.*)\z");
static GIT_HEAD: Pattern = Pattern::new(r"^(?P<head>[^/]+):(?P<last>(?!//).*)\z");
static GIT_URI: Pattern = Pattern::new(r"^(?:(?P<user>[^/@]+)@)?(?P<host>[^/:]+):(?P<path>.*)\z");
static SPACE: Pattern = Pattern::new(r"\s");
static DIGITS: Pattern = Pattern::new(r"^\d+\z");
static DOMAIN_OR_USER: Pattern = Pattern::new(r"[.@]");
static VALID_HOST: Pattern = Pattern::new(r"^[a-zA-Z0-9]+[a-zA-Z0-9\.\-]*\z");

/// `new PhutilURI($uri)` threw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUri;

/// `id(new PhutilURI($uri))->getProtocol()`.
pub fn protocol(uri: &[u8]) -> Result<Vec<u8>, InvalidUri> {
    Ok(parts(uri)?
        .and_then(|parts| parts.scheme)
        .unwrap_or_default())
}

/// Whether `id(new PhutilURI($uri))->getDomain()` is set.
pub fn has_domain(uri: &[u8]) -> bool {
    parts(uri)
        .ok()
        .flatten()
        .and_then(|parts| parts.host)
        .is_some_and(|host| !host.is_empty())
}

/// The scheme and authority of a URI: what is left of it after
/// `setPath()` and before the path, like `https://bugzilla.mozilla.org`.
pub fn origin(uri: &[u8]) -> Vec<u8> {
    let Some(separator) = uri.windows(3).position(|window| window == b"://") else {
        return Vec::new();
    };
    let authority = separator + 3;
    let end = uri[authority..]
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
        .map_or(uri.len(), |offset| authority + offset);
    uri[..end].to_vec()
}

/// What `PhutilURI` keeps of `parse_url()`, or `None` for a URI it can't
/// parse; `Err` where its constructor throws.
fn parts(uri: &[u8]) -> Result<Option<UrlParts>, InvalidUri> {
    if preg::is_match(&AMBIGUOUS, uri) {
        return Err(InvalidUri);
    }
    let parts = if let Some(groups) = preg::captures(&QUERY_ONLY, uri) {
        let mut rebuilt = groups.get(1).to_vec();
        rebuilt.push(b'/');
        rebuilt.extend_from_slice(groups.get(2));
        parse_url(&rebuilt)
    } else if is_git_uri_pattern(uri) {
        if !preg::is_match(&GIT_URI, uri) {
            return Err(InvalidUri);
        }
        Some(UrlParts {
            scheme: Some(b"ssh".to_vec()),
            host: preg::captures(&GIT_URI, uri).map(|groups| groups.get(2).to_vec()),
        })
    } else {
        parse_url(uri)
    };

    Ok(parts.filter(|_| php_ltrim(uri) == uri).filter(|parts| {
        parts
            .host
            .as_deref()
            .is_none_or(|host| host.is_empty() || preg::is_match(&VALID_HOST, host))
    }))
}

fn php_ltrim(text: &[u8]) -> &[u8] {
    crate::php::ltrim(text, crate::php::WHITESPACE)
}

/// `PhutilURI::isGitURIPattern()`.
fn is_git_uri_pattern(uri: &[u8]) -> bool {
    let Some(groups) = preg::captures(&GIT_HEAD, uri) else {
        return false;
    };
    let head = groups.get(1);
    let last = groups.get(2);
    if preg::is_match(&SPACE, head) || preg::is_match(&SPACE, last) {
        return false;
    }
    if preg::is_match(&DIGITS, last) {
        return false;
    }
    preg::is_match(&DOMAIN_OR_USER, head)
}

/// What `parse_url()` returns that matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlParts {
    scheme: Option<Vec<u8>>,
    host: Option<Vec<u8>>,
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
}

/// The port digits after a `:` at `colon`, if `parse_url()` reads them as a
/// port: one to five digits, then the end or a `/`, and at most 65535.
/// `None` means not a port; `Err` means `parse_url()` fails.
fn port_after(input: &[u8], colon: usize) -> Result<Option<()>, ()> {
    let digits = input[colon + 1..]
        .iter()
        .take(6)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let after = colon + 1 + digits;
    if digits > 0 && digits < 6 && (after == input.len() || input[after] == b'/') {
        let value: u32 = std::str::from_utf8(&input[colon + 1..after])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or(())?;
        return if value <= 65535 {
            Ok(Some(()))
        } else {
            Err(())
        };
    }
    if digits == 0 && colon + 1 == input.len() {
        return Err(());
    }
    Ok(None)
}

/// Where `parse_url()` goes after the scheme.
enum Authority {
    /// An authority (host, port) starts here.
    At {
        /// Its first byte.
        start: usize,
        /// Whether the port was already read after the first `:`.
        has_port: bool,
    },
    /// The rest is a path (`just_path` in PHP).
    PathOnly,
}

/// After a `:` at `colon` which doesn't end a scheme, `parse_url()` reads
/// a port (`parse_port`), or the rest is a path. `None` when it fails.
fn port_or_path(input: &[u8], colon: usize) -> Option<Authority> {
    let is_relative = input.starts_with(b"//");
    Some(match port_after(input, colon).ok()? {
        Some(()) => Authority::At {
            start: if is_relative { 2 } else { 0 },
            has_port: true,
        },
        None if is_relative => Authority::At {
            start: 2,
            has_port: false,
        },
        None => Authority::PathOnly,
    })
}

/// The scheme part of `php_url_parse_ex2()`: records the scheme and says
/// where the authority starts, if there is one.
fn scheme_and_authority(input: &[u8], parts: &mut UrlParts) -> Option<Authority> {
    let relative = Authority::At {
        start: 2,
        has_port: false,
    };
    let Some(colon) = input.iter().position(|byte| *byte == b':') else {
        return Some(if input.starts_with(b"//") {
            relative
        } else {
            Authority::PathOnly
        });
    };
    if colon == 0 {
        return port_or_path(input, colon);
    }
    let scheme = &input[..colon];
    if !scheme.iter().all(|byte| is_scheme_byte(*byte)) {
        let question = input
            .iter()
            .position(|byte| *byte == b'?')
            .unwrap_or(input.len());
        if colon + 1 < input.len() && colon < question {
            return port_or_path(input, colon);
        }
        return Some(if input.starts_with(b"//") {
            relative
        } else {
            Authority::PathOnly
        });
    }
    if colon + 1 == input.len() {
        parts.scheme = Some(scheme.to_vec());
        return Some(Authority::PathOnly);
    }
    if input[colon + 1] != b'/' {
        // "a.com:80" is a host and a port; "mailto:x" a scheme and a path.
        let digits = input[colon + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let after = colon + 1 + digits;
        if (after == input.len() || input[after] == b'/') && digits + 1 < 7 {
            return port_or_path(input, colon);
        }
        parts.scheme = Some(scheme.to_vec());
        return Some(Authority::PathOnly);
    }
    parts.scheme = Some(scheme.to_vec());
    if input.get(colon + 2) != Some(&b'/') {
        return Some(Authority::PathOnly);
    }
    if scheme.eq_ignore_ascii_case(b"file") && input.get(colon + 3) == Some(&b'/') {
        return Some(Authority::PathOnly);
    }
    Some(Authority::At {
        start: colon + 3,
        has_port: false,
    })
}

/// The host part of `php_url_parse_ex2()`: the authority without the user
/// and the port. `None` when `parse_url()` fails.
fn host(input: &[u8], start: usize, has_port: bool) -> Option<Vec<u8>> {
    let end = input[start..]
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
        .map_or(input.len(), |offset| start + offset);
    let host_start = input[start..end]
        .iter()
        .rposition(|byte| *byte == b'@')
        .map_or(start, |at| start + at + 1);
    let authority = &input[host_start..end];
    let is_ipv6 = authority.first() == Some(&b'[') && authority.last() == Some(&b']');
    let host_end = match authority.iter().rposition(|byte| *byte == b':') {
        Some(colon) if !is_ipv6 => {
            if !has_port {
                let port = &authority[colon + 1..];
                if port.len() > 5 {
                    return None;
                }
                if !port.is_empty() {
                    let digits = port.iter().take_while(|byte| byte.is_ascii_digit()).count();
                    let value: u32 = std::str::from_utf8(&port[..digits]).ok()?.parse().ok()?;
                    if value > 65535 {
                        return None;
                    }
                }
            }
            host_start + colon
        }
        _ => end,
    };
    (host_end > host_start).then(|| input[host_start..host_end].to_vec())
}

/// `parse_url()`, for the scheme and the host. `None` when PHP returns
/// `false`.
fn parse_url(input: &[u8]) -> Option<UrlParts> {
    let mut parts = UrlParts {
        scheme: None,
        host: None,
    };
    if let Authority::At { start, has_port } = scheme_and_authority(input, &mut parts)? {
        parts.host = Some(host(input, start, has_port)?);
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("http://www.example.com/", Ok("http"))]
    #[case("https://example.com:8080/x?y#z", Ok("https"))]
    #[case("mailto:alincoln@example.com", Ok("mailto"))]
    #[case("tel:18005555555", Ok("tel"))]
    #[case("tel:123", Ok(""))]
    #[case("javascript://www.example.com/", Ok("javascript"))]
    #[case("/x/", Ok(""))]
    #[case("http://exa_mple.com/", Ok(""))]
    #[case("http:///x", Ok(""))]
    #[case(" http://example.com/", Ok(""))]
    #[case("http://example.com?x=1:2", Err(InvalidUri))]
    #[case("git@github.com:foo/bar.git", Ok("ssh"))]
    fn protocols(#[case] uri: &str, #[case] expected: Result<&str, InvalidUri>) {
        assert_eq!(
            protocol(uri.as_bytes()),
            expected.map(|scheme| scheme.as_bytes().to_vec())
        );
    }
}
