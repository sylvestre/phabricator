//! Diffusion URIs: the request blobs PHP parses
//! (`DiffusionRequest::parseRequestBlob()`) and the URIs it generates
//! (`PhabricatorRepository::generateURI()`).

use std::fmt::Write as _;

use phab_app_differential::repository::Repository;

/// The parts of a blob like `master/dir/file.c;abcdef$12`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Blob {
    pub(crate) branch: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) line: Option<String>,
}

/// `rawurldecode()`.
pub(crate) fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `rawurlencode()`.
fn raw_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// `phutil_escape_uri()`: `rawurlencode()` keeping slashes.
pub(crate) fn escape(text: &str) -> String {
    raw_encode(text).replace("%2F", "/")
}

/// `phutil_escape_uri_path_component()`: encoded twice, so a slash
/// survives the web server's decoding.
fn escape_path_component(text: &str) -> String {
    raw_encode(&raw_encode(text))
}

/// The line range ending a blob (`[$]([\d,-]+)$` after an even run of
/// `$`): the text before it, and the range.
fn split_line(text: &str) -> Option<(&str, &str)> {
    let dollar = text.rfind('$')?;
    let line = &text[dollar + 1..];
    if line.is_empty()
        || !line
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b',' || byte == b'-')
    {
        return None;
    }
    let run = text.as_bytes()[..dollar]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'$')
        .count();
    (run % 2 == 0).then(|| (&text[..dollar], line))
}

/// The commit of a blob (`(?:^|[^;])(?:;;)*;([^;].*)$`, the leftmost
/// match): the text before it, and the commit.
fn split_commit(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] != b';' {
            position += 1;
            continue;
        }
        let run = bytes[position..]
            .iter()
            .take_while(|byte| **byte == b';')
            .count();
        let end = position + run;
        // An odd run ends with the separator, followed by a non-";".
        if run % 2 == 1 && end < bytes.len() {
            return Some((&text[..end - 1], &text[end..]));
        }
        position = end;
    }
    None
}

/// `parseRequestBlob()` for a repository with branches; `None` for a
/// path with a `..` part (PHP throws).
pub(crate) fn parse_blob(blob: &str) -> Option<Blob> {
    let mut result = Blob::default();
    let mut rest = blob;
    if let Some(slash) = rest.find('/')
        && slash > 0
    {
        result.branch = Some(decode(&rest[..slash]));
        rest = &rest[slash + 1..];
    }
    if let Some((before, line)) = split_line(rest) {
        result.line = Some(line.to_owned());
        rest = before;
    }
    let unescaped = rest.replace("$$", "$");
    let mut rest = unescaped.as_str();
    if let Some((before, commit)) = split_commit(rest) {
        result.commit = Some(commit.to_owned());
        rest = before;
    }
    let blob = rest.replace(";;", ";");
    if !blob.is_empty() {
        if blob.split('/').any(|part| part == "..") {
            return None;
        }
        result.path = Some(blob);
    }
    Some(result)
}

/// What a generated URI points at (`generateURI()`'s parameters).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Target<'a> {
    pub(crate) path: &'a str,
    pub(crate) branch: &'a str,
    pub(crate) commit: &'a str,
    pub(crate) line: &'a str,
}

/// `generateURI()` for the actions the pages link to: `browse`,
/// `history`, `branches`, `tags`, `change`, `compare`, `clone`, `branch`
/// and the others which take a path.
pub(crate) fn generate(repository: &Repository, action: &str, target: Target<'_>) -> String {
    let mut path = String::new();
    if !target.path.is_empty() {
        path = escape(
            &target
                .path
                .trim_start_matches('/')
                .replace(';', ";;")
                .replace('$', "$$"),
        );
    }
    if !target.branch.is_empty() {
        path = format!("{}/{path}", escape_path_component(target.branch));
    }
    let commit = if target.commit.is_empty() {
        String::new()
    } else {
        format!(";{}", escape(&target.commit.replace('$', "$$")))
    };
    let line = if target.line.is_empty() {
        String::new()
    } else {
        format!("${}", escape(target.line))
    };
    let base = repository.uri();
    match action {
        "branch" if path.is_empty() => base,
        "branch" => format!("{base}repository/{path}"),
        "clone" | "compare" => format!("{base}{action}/"),
        _ => format!("{base}{action}/{path}{commit}{line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(branch: Option<&str>, path: Option<&str>, commit: Option<&str>) -> Blob {
        Blob {
            branch: branch.map(str::to_owned),
            path: path.map(str::to_owned),
            commit: commit.map(str::to_owned),
            line: None,
        }
    }

    #[test]
    fn parses_blobs_like_php() {
        assert_eq!(
            parse_blob("master/"),
            Some(blob(Some("master"), None, None))
        );
        assert_eq!(
            parse_blob("master/dir/a.txt"),
            Some(blob(Some("master"), Some("dir/a.txt"), None))
        );
        assert_eq!(
            parse_blob("feature%2Fx/a;v1"),
            Some(blob(Some("feature/x"), Some("a"), Some("v1")))
        );
        assert_eq!(
            parse_blob("master/money/$$100;;x"),
            Some(blob(Some("master"), Some("money/$100;x"), None))
        );
        let mut with_line = blob(Some("master"), Some("README"), Some("abc"));
        with_line.line = Some("2-3".to_owned());
        assert_eq!(parse_blob("master/README;abc$2-3"), Some(with_line));
        assert_eq!(parse_blob(";abc"), Some(blob(None, None, Some("abc"))));
        assert_eq!(parse_blob("master/a/../b"), None);
    }

    #[test]
    fn encodes_like_php() {
        assert_eq!(escape("a b/c;d"), "a%20b/c%3Bd");
        assert_eq!(escape_path_component("a/b"), "a%252Fb");
        assert_eq!(decode("a%2Fb%20c"), "a/b c");
    }
}
