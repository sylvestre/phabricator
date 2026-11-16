//! `DiffusionSourceLinkRemarkupRule` (`{src path repository=...}`) and
//! `DiffusionSourceHyperlinkEngineExtension` (links to files in Diffusion):
//! links to a repository's files. They go to Searchfox when the
//! repository has a tree there, to Diffusion otherwise.

use crate::engine::{Context, SourceLink};
use crate::markup::{self, Content, Markup};
use crate::objects::RepositoryRef;
use crate::options::{self, OptionValue};
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};
use crate::rules::icon;
use crate::uri;

static SOURCE: Pattern = Pattern::new(r"(?m)\{(?:src|source)\b((?:[^}\\]+|\\.)*)\}");
// The "\." is a literal dot, as in PHP.
static INPUT: Pattern = Pattern::new(
    r#"^[\s,]*(?:"(?P<quotedpath>(?:[^\\"]+|\.)+)"|(?P<rawpath>[^\s,]+))[\s,]*(?P<options>.*)\z"#,
);
/// The path of a link to a file in Diffusion.
pub static BROWSE_PATH: Pattern =
    Pattern::new(r"^/(?:diffusion|source)/(?P<identifier>[^/]+)/browse/(?P<blob>.*)\z");
static BRANCH: Pattern = Pattern::new(r"^([^/]+)/");
static LINE: Pattern = Pattern::new(r"(?:(?:^|[^$])(?:[$][$])*)[$]([\d,-]+)$");
static COMMIT: Pattern = Pattern::new(r"(?:(?:^|[^;])(?:;;)*);([^;].*)$");

/// `DiffusionSourceLinkRemarkupRule`.
pub struct SourceLinkRule;

/// Where a link points in a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    /// The file.
    pub path: Option<Vec<u8>>,
    /// The line or lines, like `12` or `12-14`.
    pub line: Option<Vec<u8>>,
    /// The commit.
    pub commit: Option<Vec<u8>>,
    /// The branch.
    pub branch: Option<Vec<u8>>,
}

/// `DiffusionRequest::parseRequestBlob()`.
pub fn parse_blob(blob: &[u8], supports_branches: bool) -> Location {
    let mut location = Location::default();
    let mut blob = blob.to_vec();
    if supports_branches && let Some(groups) = preg::captures(&BRANCH, &blob) {
        let branch = groups.get(1).to_vec();
        location.branch = Some(unescape_path_component(&branch));
        blob.drain(..=branch.len());
    }
    if let Some(groups) = preg::captures(&LINE, &blob) {
        let line = groups.get(1).to_vec();
        blob.truncate(blob.len() - line.len() - 1);
        location.line = Some(line);
    }
    let mut blob = replace(&blob, b"$$", b"$");
    if let Some(groups) = preg::captures(&COMMIT, &blob) {
        let commit = groups.get(1).to_vec();
        blob.truncate(blob.len() - commit.len() - 1);
        location.commit = Some(commit);
    }
    let blob = replace(&blob, b";;", b";");
    if !blob.is_empty() {
        location.path = Some(blob);
    }
    location
}

fn replace(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index..].starts_with(from) {
            output.extend_from_slice(to);
            index += from.len();
        } else {
            output.push(text[index]);
            index += 1;
        }
    }
    output
}

/// `phutil_unescape_uri_path_component()`: `rawurldecode()` twice.
fn unescape_path_component(text: &[u8]) -> Vec<u8> {
    let once = uri::urldecode(&replace(text, b"+", b"%2B"));
    uri::urldecode(&replace(&once, b"+", b"%2B"))
}

/// `phutil_escape_uri_path_component()`: `rawurlencode()` twice.
fn escape_path_component(text: &[u8]) -> Vec<u8> {
    uri::rawurlencode(&uri::rawurlencode(text))
}

/// Where `location` is shown: Searchfox, or Diffusion's browse page
/// (`PhabricatorRepository::generateURI()`).
pub fn browse_uri(repository: &RepositoryRef, location: &Location) -> Vec<u8> {
    let path = location.path.as_deref().unwrap_or_default();
    let path = path.strip_prefix(b"/").unwrap_or(path);
    if let Some(searchfox) = &repository.searchfox {
        let mut href = searchfox.clone();
        match &location.commit {
            Some(commit) => {
                href.extend_from_slice(b"rev/");
                href.extend_from_slice(&uri::rawurlencode(commit));
                href.push(b'/');
            }
            None => href.extend_from_slice(b"source/"),
        }
        href.extend_from_slice(&uri::escape_uri(path));
        if let Some(line) = &location.line {
            href.push(b'#');
            href.extend_from_slice(&uri::rawurlencode(line));
        }
        return href;
    }
    let mut href = repository.uri.clone();
    href.extend_from_slice(b"browse/");
    if let Some(branch) = location
        .branch
        .as_ref()
        .or(repository.default_branch.as_ref())
    {
        href.extend_from_slice(&escape_path_component(branch));
        href.push(b'/');
    }
    let path = replace(&replace(path, b"$", b"$$"), b";", b";;");
    href.extend_from_slice(&uri::escape_uri(&path));
    if let Some(commit) = &location.commit {
        href.push(b';');
        href.extend_from_slice(&uri::escape_uri(&replace(commit, b"$", b"$$")));
    }
    if let Some(line) = &location.line {
        href.push(b'$');
        href.extend_from_slice(&uri::escape_uri(line));
    }
    href
}

/// `DiffusionSourceLinkView`: the link's text, unless `text` is given, is
/// the repository, the path and the line, and the branch or commit.
pub fn render_view(
    repository: &RepositoryRef,
    href: &[u8],
    location: &Location,
    text: Option<&[u8]>,
) -> Markup {
    let text = text.map_or_else(
        || {
            let mut path = location.path.clone().unwrap_or_default();
            if let Some(line) = &location.line {
                path = [&path[..], b":", line].concat();
            }
            let path = [&repository.monogram[..], b" ", &path].concat();
            let branch = location
                .branch
                .as_ref()
                .filter(|branch| repository.default_branch.as_ref() != Some(*branch));
            match (branch, &location.commit) {
                (Some(branch), Some(commit)) => {
                    [&path[..], b" (on ", branch, b" at ", commit, b")"].concat()
                }
                (Some(branch), None) => [&path[..], b" (on ", branch, b")"].concat(),
                (None, Some(commit)) => [&path[..], b" (at ", commit, b")"].concat(),
                (None, None) => path,
            }
        },
        <[u8]>::to_vec,
    );
    let content = [
        Some(icon::render(b"file-text-o", None, false)),
        Some(Markup::text(text)),
    ];
    markup::tag(
        "a",
        &[
            ("href", Some(href)),
            ("class", Some(b"remarkup-source-link")),
            ("data-phid", Some(repository.phid.as_bytes())),
        ],
        Content::List(&content),
    )
}

/// `newSyntaxHint()`: what is wrong with a `{src}`.
fn syntax_hint(text: &[u8]) -> Markup {
    let content = [
        Some(icon::render(b"exclamation-triangle", None, false)),
        Some(Markup::text(text)),
    ];
    markup::tag(
        "span",
        &[("class", Some(b"remarkup-syntax-hint"))],
        Content::List(&content),
    )
}

/// `stripcslashes()`.
fn strip_c_slashes(text: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] != b'\\' || index + 1 == text.len() {
            output.push(text[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = text[index];
        index += 1;
        output.push(match escaped {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'a' => 0x07,
            b'v' => 0x0b,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'x' if text.get(index).is_some_and(u8::is_ascii_hexdigit) => {
                let end = (index + 2).min(text.len());
                let end = index
                    + text[index..end]
                        .iter()
                        .take_while(|byte| byte.is_ascii_hexdigit())
                        .count();
                let value = u8::from_str_radix(
                    std::str::from_utf8(&text[index..end]).expect("hex digits"),
                    16,
                )
                .expect("at most two hex digits");
                index = end;
                value
            }
            b'0'..=b'7' => {
                let start = index - 1;
                let end = start
                    + text[start..(start + 3).min(text.len())]
                        .iter()
                        .take_while(|byte| (b'0'..=b'7').contains(*byte))
                        .count();
                let value = u32::from_str_radix(
                    std::str::from_utf8(&text[start..end]).expect("octal digits"),
                    8,
                )
                .expect("at most three octal digits");
                index = end;
                (value & 0xff) as u8
            }
            other => other,
        });
    }
    output
}

impl InlineRule for SourceLinkRule {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&SOURCE, text.as_bytes(), |groups| {
            // Text and mail could do better; PHP focuses on the web.
            if !rule::is_flat_text(groups.get(0)) || cx.is_text_mode() || cx.is_html_mail_mode() {
                return groups.get(0).to_vec();
            }
            let raw = groups.get(0).to_vec();
            let token = cx.store_text(Markup::text(raw.clone()));
            let parsed = parse_source(&raw, groups.get(1));
            cx.metadata.source_links.push(SourceLink {
                token: token.clone(),
                parsed,
            });
            token
        }))
    }

    fn did_markup_text(&self, cx: &mut Context<'_>) {
        for link in std::mem::take(&mut cx.metadata.source_links) {
            let view = match &link.parsed {
                Err(hint) => syntax_hint(hint),
                Ok((identifier, location)) => {
                    // Repositories the viewer can't see stay as written.
                    let Some(repository) = cx.resolved.repositories.get(identifier) else {
                        continue;
                    };
                    let href = browse_uri(repository, location);
                    render_view(repository, &href, location, None)
                }
            };
            cx.overwrite_stored_text(&link.token, view);
        }
    }
}

/// The repository identifier and the location of a `{src}`, or the hint
/// to show instead.
pub type ParsedSource = Result<(Vec<u8>, Location), Vec<u8>>;

/// The first part of `didMarkupText()`: reads a `{src}`.
fn parse_source(raw: &[u8], input: &[u8]) -> ParsedSource {
    let Some(groups) = preg::captures(&INPUT, input) else {
        return Err([&b"Missing path, expected \"{src path ...}\" in: "[..], raw].concat());
    };
    let path = if groups.get(2).is_empty() {
        strip_c_slashes(groups.get(1))
    } else {
        groups.get(2).to_vec()
    };
    let (mut repository, path) = match path.iter().position(|byte| *byte == b':') {
        Some(colon) => (
            Some(path[..colon].to_vec()).filter(|name| !name.is_empty() && name != b"0"),
            path[colon + 1..].to_vec(),
        ),
        None => (None, path),
    };
    let mut location = Location {
        path: Some(path),
        ..Location::default()
    };
    for (key, value) in options::parse(groups.get(3)) {
        let value = match value {
            OptionValue::Flag => b"1".to_vec(),
            OptionValue::Value(value) => value,
        };
        match &key[..] {
            b"repository" => repository = Some(value),
            b"line" => location.line = Some(value),
            b"commit" => location.commit = Some(value),
            b"ref" => location.branch = Some(value),
            _ => {
                return Err([&b"Unknown option \""[..], &key, b"\" in: ", raw].concat());
            }
        }
    }
    let Some(repository) = repository else {
        return Err([
            &b"Missing repository, expected \"{src repository:path ...}\" or \"{src path repository=...}\" in: "[..],
            raw,
        ]
        .concat());
    };
    Ok((repository, location))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_parse_like_diffusion() {
        assert_eq!(
            parse_blob(b"default/dom/a.cpp;abc$12-14", true),
            Location {
                path: Some(b"dom/a.cpp".to_vec()),
                line: Some(b"12-14".to_vec()),
                commit: Some(b"abc".to_vec()),
                branch: Some(b"default".to_vec()),
            }
        );
        assert_eq!(
            parse_blob(b"money/$$100", false).path,
            Some(b"money/$100".to_vec())
        );
    }

    #[test]
    fn sources_name_a_repository() {
        let (repository, location) = parse_source(b"", b" mc:dom/a.cpp, line=3").unwrap();
        assert_eq!(repository, b"mc");
        assert_eq!(location.path.as_deref(), Some(&b"dom/a.cpp"[..]));
        assert_eq!(location.line.as_deref(), Some(&b"3"[..]));
        assert!(parse_source(b"x", b"a.cpp").is_err());
        assert!(parse_source(b"x", b"a.cpp, colour=red").is_err());
    }
}
