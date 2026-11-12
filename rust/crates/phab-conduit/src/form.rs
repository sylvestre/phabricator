//! Port of `PhutilQueryStringParser`, which Phabricator uses instead of PHP's
//! own `$_GET`/`$_POST` parsing (so `api.token` keeps its dot).

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// An ordered PHP array built from `a[b][]=c` style keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormValue {
    /// A plain value.
    Str(String),
    /// A nested array.
    Array(IndexMap<String, Self>),
}

impl FormValue {
    /// The string, or `None` for an array.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            Self::Array(_) => None,
        }
    }

    /// The value as PHP's `json_encode()` would see it: arrays with keys
    /// `0..n` in order become lists, other arrays objects.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Str(text) => Value::String(text.clone()),
            Self::Array(entries) => {
                let is_list = entries
                    .keys()
                    .enumerate()
                    .all(|(index, key)| *key == index.to_string());
                if is_list {
                    Value::Array(entries.values().map(Self::to_json).collect())
                } else {
                    Value::Object(
                        entries
                            .iter()
                            .map(|(key, value)| (key.clone(), value.to_json()))
                            .collect::<Map<_, _>>(),
                    )
                }
            }
        }
    }
}

/// Request data: an ordered map from names to values. Like a PHP array, a
/// key set again keeps its position.
pub type FormData = IndexMap<String, FormValue>;

/// `urldecode()`: `+` is a space and invalid escapes are kept verbatim.
fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit) =>
            {
                let hex = &text[index + 1..index + 3];
                output.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                index += 2;
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// Keys like `a[b][]`: a name without brackets followed by bracket groups.
fn key_pieces(key: &str) -> Option<Vec<&str>> {
    let open = key.find('[')?;
    let (name, mut rest) = key.split_at(open);
    if name.is_empty() || name.contains(']') {
        return None;
    }
    let mut pieces = vec![name];
    while !rest.is_empty() {
        let inner = rest.strip_prefix('[')?;
        let close = inner.find(']')?;
        let piece = &inner[..close];
        if piece.contains('[') {
            return None;
        }
        pieces.push(piece);
        rest = &inner[close + 1..];
    }
    Some(pieces)
}

fn next_index(entries: &FormData) -> String {
    entries
        .keys()
        .filter_map(|key| key.parse::<i64>().ok())
        .max()
        .map_or(0, |max| max + 1)
        .to_string()
}

fn insert_nested(entries: &mut FormData, pieces: &[&str], value: String) {
    let Some((first, rest)) = pieces.split_first() else {
        return;
    };
    let key = if first.is_empty() {
        next_index(entries)
    } else {
        (*first).to_owned()
    };
    if rest.is_empty() {
        entries.insert(key, FormValue::Str(value));
        return;
    }
    let child = entries
        .entry(key)
        .or_insert_with(|| FormValue::Array(IndexMap::new()));
    // PHP replaces anything that is not a non-empty array.
    if !matches!(child, FormValue::Array(items) if !items.is_empty()) {
        *child = FormValue::Array(IndexMap::new());
    }
    if let FormValue::Array(children) = child {
        insert_nested(children, rest, value);
    }
}

/// Parses a query string or form-encoded body. Later values of a key
/// replace earlier ones.
pub fn parse_query_string(query: &str) -> FormData {
    let mut data = FormData::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = url_decode(raw_key);
        let value = url_decode(raw_value);
        if key.is_empty() {
            continue;
        }
        match key_pieces(&key) {
            Some(pieces) => insert_nested(&mut data, &pieces, value),
            None => {
                data.insert(key, FormValue::Str(value));
            }
        }
    }
    data
}

/// `$_POST + $_GET`: POST values win.
pub fn merge(post: FormData, get: FormData) -> FormData {
    let mut data = post;
    for (key, value) in get {
        data.entry(key).or_insert(value);
    }
    data
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parsed(query: &str) -> Value {
        Value::Object(
            parse_query_string(query)
                .iter()
                .map(|(key, value)| (key.clone(), value.to_json()))
                .collect(),
        )
    }

    #[test]
    fn parses_like_phutil() {
        assert_eq!(
            parsed("api.token=abc&a+b=c%20d&empty&=skipped&x=1&x=2"),
            json!({"api.token": "abc", "a b": "c d", "empty": "", "x": "2"})
        );
    }

    #[test]
    fn builds_arrays() {
        assert_eq!(
            parsed("ids[]=4&ids[]=5&params[limit]=3&params[view]=%22data%22&m[a][]=x"),
            json!({
                "ids": ["4", "5"],
                "params": {"limit": "3", "view": "\"data\""},
                "m": {"a": ["x"]},
            })
        );
    }

    #[test]
    fn malformed_brackets_are_plain_keys() {
        assert_eq!(
            parsed("a[b=1&[c]=2&d[e]f=3&g]=4"),
            json!({"a[b": "1", "[c]": "2", "d[e]f": "3", "g]": "4"})
        );
    }

    #[test]
    fn scalars_are_replaced_by_arrays() {
        assert_eq!(parsed("a=1&a[x]=2"), json!({"a": {"x": "2"}}));
        assert_eq!(parsed("a[x]=2&a=1"), json!({"a": "1"}));
    }

    #[test]
    fn keeps_invalid_escapes() {
        assert_eq!(parsed("a=%zz%4"), json!({"a": "%zz%4"}));
    }

    #[test]
    fn post_wins_over_get() {
        let data = merge(
            parse_query_string("a=post"),
            parse_query_string("a=get&b=get"),
        );
        assert_eq!(data.get("a").and_then(FormValue::as_str), Some("post"));
        assert_eq!(data.get("b").and_then(FormValue::as_str), Some("get"));
    }
}
