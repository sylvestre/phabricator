//! Repository URIs as `PhutilURI` reads and writes them, for the URIs of
//! `PhabricatorRepositoryURI`: `PhabricatorRepository::assertValidRemoteURI()`,
//! the effective URI (`getURIObject()` of a URI which is not builtin) and
//! its normalized form (`ArcanistRepositoryURINormalizer`, for Git).
//!
//! Only the URIs `PhutilURI` parses the same way are read:
//! `scheme://[user@]host[:port][/path]` and Git's `user@host:path`, with
//! plain user names and paths, without password, query or fragment.
//! Anything else is `None`, and left to PHP.

/// A parsed URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUri {
    /// `getProtocol()`: `ssh` for Git URIs.
    pub protocol: String,
    /// `getUser()`, maybe empty.
    pub user: String,
    /// `getDomain()`, maybe empty.
    pub domain: String,
    /// `getPort()`, maybe empty.
    pub port: String,
    /// `getPath()`, maybe empty.
    pub path: String,
    /// `isGitURI()`.
    pub git: bool,
}

/// Characters of user names `rawurldecode()` and `rawurlencode()` keep.
fn is_plain_user(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
}

/// Characters of paths every step reads alike.
fn is_plain_path(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._~+-".contains(&byte))
}

/// `^[a-zA-Z0-9]+[a-zA-Z0-9\.\-]*\z`, which `PhutilURI` requires of hosts.
fn is_valid_host(host: &str) -> bool {
    host.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

/// `PhutilURI::isGitURIPattern()`: `^([^/]+):((?!//).*)\z`, the head
/// greedy, then the checks on both parts.
fn is_git_pattern(uri: &str) -> bool {
    let run = uri.find('/').map_or(uri, |slash| &uri[..slash]);
    let Some(colon) = run.rfind(':').filter(|colon| *colon > 0) else {
        return false;
    };
    let (head, last) = (&uri[..colon], &uri[colon + 1..]);
    if last.starts_with("//") {
        return false;
    }
    if (head.bytes().chain(last.bytes())).any(|byte| byte.is_ascii_whitespace()) {
        return false;
    }
    if !last.is_empty() && last.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    head.contains(['.', '@'])
}

impl RemoteUri {
    /// `new PhutilURI($uri)`, for the URIs it reads like this parser.
    pub fn parse(uri: &str) -> Option<Self> {
        if !uri.is_ascii()
            || uri
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || uri.contains(['?', '#', '%'])
        {
            return None;
        }
        if is_git_pattern(uri) {
            return Self::parse_git(uri);
        }
        let (protocol, rest) = uri.split_once("://")?;
        let mut letters = protocol.bytes();
        if !letters
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
            || !letters.all(|byte| byte.is_ascii_alphanumeric() || b"+.-".contains(&byte))
        {
            return None;
        }
        let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
        if !is_plain_path(path) {
            return None;
        }
        let (user, host_port) = match authority.split_once('@') {
            Some((user, host_port)) if is_plain_user(user) => (user, host_port),
            Some(_) => return None,
            None => ("", authority),
        };
        let (domain, port) = match host_port.split_once(':') {
            Some((domain, port)) => {
                let valid = !port.starts_with('0')
                    && port.len() <= 5
                    && port.bytes().all(|byte| byte.is_ascii_digit())
                    && port
                        .parse::<u32>()
                        .is_ok_and(|port| (1..=65535).contains(&port));
                if !valid {
                    return None;
                }
                (domain, port)
            }
            None => (host_port, ""),
        };
        if domain.is_empty() {
            // `parse_url()` reads `file:///path` without a host, and fails
            // on the other protocols.
            if protocol != "file" || !user.is_empty() || !port.is_empty() || path.is_empty() {
                return None;
            }
        } else if !is_valid_host(domain) {
            return None;
        }
        Some(Self {
            protocol: protocol.to_owned(),
            user: user.to_owned(),
            domain: domain.to_owned(),
            port: port.to_owned(),
            path: path.to_owned(),
            git: false,
        })
    }

    /// `(?:(?P<user>[^/@]+)@)?(?P<host>[^/:]+):(?P<path>.*)`.
    fn parse_git(uri: &str) -> Option<Self> {
        let (user, rest) = match uri.split_once('@') {
            Some((user, rest)) if !user.contains('/') => (user, rest),
            _ => ("", uri),
        };
        if uri.contains('@') && !is_plain_user(user) {
            return None;
        }
        let (host, path) = rest.split_once(':')?;
        if !is_valid_host(host) || !is_plain_path(path) {
            return None;
        }
        Some(Self {
            protocol: "ssh".to_owned(),
            user: user.to_owned(),
            domain: host.to_owned(),
            port: String::new(),
            path: path.to_owned(),
            git: true,
        })
    }

    /// `getEffectiveURI()` of a URI which is not builtin: the default port
    /// of the protocol removed.
    #[must_use]
    pub fn effective(mut self) -> Self {
        let default = match self.protocol.as_str() {
            "ssh" => "22",
            "http" => "80",
            "https" => "443",
            _ => "",
        };
        if !default.is_empty() && self.port == default {
            self.port.clear();
        }
        self
    }

    /// `PhutilURI::__toString()`.
    pub fn to_uri_string(&self) -> String {
        let auth = if self.user.is_empty() {
            String::new()
        } else {
            format!("{}@", self.user)
        };
        if self.git {
            let path = if self.path.is_empty() {
                String::new()
            } else {
                format!(":{}", self.path)
            };
            return format!("{auth}{}{path}", self.domain);
        }
        let port = if self.port.is_empty() {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        format!(
            "{}://{auth}{}{port}{}",
            self.protocol, self.domain, self.path
        )
    }
}

/// `ArcanistRepositoryURINormalizer::getNormalizedURI()` of the Git URI
/// `display` (which it parses again), `domains` being the domain map
/// (`<base-uri>`, then `<ssh-host>`), lowercased. `None` for URIs this
/// module doesn't read.
pub fn normalized_git(display: &str, domains: &[(&str, String)]) -> Option<String> {
    let uri = RemoteUri::parse(display)?;
    let domain = if uri.domain.is_empty() {
        "<void>".to_owned()
    } else {
        let domain = uri.domain.to_ascii_lowercase();
        domains
            .iter()
            .find(|(_, value)| *value == domain)
            .map_or(domain, |(key, _)| (*key).to_owned())
    };
    let path = uri.path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    Some(format!("{domain}/{}", phabricator_path(path)))
}

/// `^(diffusion/(?:[A-Z]+|\d+))`: a Phabricator path stripped to its
/// callsign or ID.
fn phabricator_path(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("diffusion/") else {
        return path;
    };
    let upper = rest.bytes().take_while(u8::is_ascii_uppercase).count();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    match upper.max(digits) {
        0 => path,
        length => &path[.."diffusion/".len() + length],
    }
}

/// `PhabricatorRepository::assertValidRemoteURI()`: the message of its
/// exception, if any. `None` for URIs this module doesn't read.
pub fn validation_error(uri: &str) -> Option<Option<String>> {
    if uri.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B']) != uri {
        return Some(Some(
            "The remote URI has leading or trailing whitespace.".to_owned(),
        ));
    }
    let parsed = RemoteUri::parse(uri)?;
    // The parser refuses the `proto://domain:/path` URIs PHP explains: their
    // port is not a number.
    match parsed.protocol.as_str() {
        "ssh" | "http" | "https" | "git" | "svn" | "svn+ssh" => Some(None),
        _ => Some(Some(
            "The URI protocol is unrecognized. It should begin with \"ssh://\", \"http://\", \
             \"https://\", \"git://\", \"svn://\", \"svn+ssh://\", or be in the form \
             \"git@domain.com:path\"."
                .to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(uri: &str) -> Option<String> {
        RemoteUri::parse(uri).map(|parsed| parsed.effective().to_uri_string())
    }

    #[test]
    fn writes_uris_like_phutil_uri() {
        for uri in [
            "https://github.com/mozilla/gecko-dev.git",
            "ssh://git@github.com:2222/a/b",
            "git@github.com:mozilla/gecko-dev.git",
            "github.com:mozilla/x",
            "https://example.com",
            "file:///var/repo",
        ] {
            assert_eq!(round_trip(uri).as_deref(), Some(uri), "{uri}");
        }
        assert_eq!(
            round_trip("https://example.com:443/x").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(round_trip("ssh://h:22/x").as_deref(), Some("ssh://h/x"));
        for uri in [
            "https://user:pass@host/x",
            "https://host/x?y",
            "ssh://host:/x",
            "ssh://git@github.com:org/repo.git",
            "https://-host/x",
            "https:///x",
            "localhost:path",
            "host:123",
            " https://host/",
            "https://host/a b",
        ] {
            assert_eq!(RemoteUri::parse(uri), None, "{uri}");
        }
    }

    #[test]
    fn normalizes_like_arcanist() {
        let domains = [("<base-uri>", "phabricator.example.com".to_owned())];
        let normal = |uri: &str| {
            let display = RemoteUri::parse(uri)
                .expect("parsed")
                .effective()
                .to_uri_string();
            normalized_git(&display, &domains).expect("normalized")
        };
        assert_eq!(
            normal("https://GitHub.com/mozilla/gecko-dev.git/"),
            "github.com/mozilla/gecko-dev"
        );
        assert_eq!(
            normal("git@github.com:mozilla/gecko-dev.git"),
            "github.com/mozilla/gecko-dev"
        );
        assert_eq!(
            normal("https://phabricator.example.com/diffusion/MOZ/x.git"),
            "<base-uri>/diffusion/MOZ"
        );
        assert_eq!(
            normal("https://phabricator.example.com/diffusion/12/x.git"),
            "<base-uri>/diffusion/12"
        );
        assert_eq!(normal("file:///var/repo"), "<void>/var/repo");
        // Read again, "host.com" would be a path.
        assert_eq!(normalized_git("host.com", &domains), None);
    }

    #[test]
    fn validates_like_php() {
        assert_eq!(validation_error("https://github.com/a/b"), Some(None));
        assert_eq!(validation_error("git@github.com:a/b"), Some(None));
        assert!(
            validation_error(" https://github.com/a/b")
                .is_some_and(|error| error.is_some_and(|error| error.contains("whitespace")))
        );
        assert!(
            validation_error("file:///var/repo")
                .is_some_and(|error| error.is_some_and(|error| error.contains("unrecognized")))
        );
        assert_eq!(validation_error("https://host/x?y"), None);
    }
}
