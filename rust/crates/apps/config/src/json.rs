//! JSON as PHP's configuration code reads and writes it: `PhutilJSON::
//! encodeFormatted()` for `local.json`, and `phutil_json_decode()`.

use serde_json::{Map, Value};

/// Whether `map` is what PHP sees as a list: keys `0..n` in order (or no
/// keys at all), the arrays `phutil_is_natural_list()` accepts.
fn is_natural_list(map: &Map<String, Value>) -> bool {
    map.keys()
        .enumerate()
        .all(|(index, key)| *key == index.to_string())
}

/// `json_encode($value, JSON_UNESCAPED_SLASHES)` of a scalar.
fn encode_scalar(value: &Value) -> String {
    phab_core::php::json_encode(value).replace("\\/", "/")
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn encode_object<'a>(entries: impl Iterator<Item = (&'a str, &'a Value)>, depth: usize) -> String {
    let lines: Vec<String> = entries
        .map(|(key, value)| {
            format!(
                "{}{}: {}",
                indent(depth + 1),
                encode_scalar(&Value::String(key.to_owned())),
                encode_value(value, depth + 1)
            )
        })
        .collect();
    if lines.is_empty() {
        return "{}".to_owned();
    }
    format!("{{\n{}\n{}}}", lines.join(",\n"), indent(depth))
}

fn encode_list<'a>(values: impl Iterator<Item = &'a Value>, depth: usize) -> String {
    let lines: Vec<String> = values
        .map(|value| format!("{}{}", indent(depth + 1), encode_value(value, depth + 1)))
        .collect();
    if lines.is_empty() {
        return "[]".to_owned();
    }
    format!("[\n{}\n{}]", lines.join(",\n"), indent(depth))
}

/// `encodeFormattedValue()`: PHP arrays which are natural lists print as
/// lists, so an empty object is `[]` below the top level.
fn encode_value(value: &Value, depth: usize) -> String {
    match value {
        Value::Array(list) => encode_list(list.iter(), depth),
        Value::Object(map) if is_natural_list(map) => encode_list(map.values(), depth),
        Value::Object(map) => encode_object(map.iter().map(|(k, v)| (k.as_str(), v)), depth),
        scalar => encode_scalar(scalar),
    }
}

/// `PhutilJSON::encodeFormatted()`: the object pretty-printed with two
/// spaces, and a final newline.
pub fn encode_formatted(object: &Map<String, Value>) -> String {
    let mut output = encode_object(object.iter().map(|(k, v)| (k.as_str(), v)), 0);
    output.push('\n');
    output
}

/// `PhutilJSON::encodeFormatted()` of a list (`encodeAsList()`).
pub fn encode_list_formatted(list: &[Value]) -> String {
    let mut output = encode_list(list.iter(), 0);
    output.push('\n');
    output
}

/// `phutil_json_decode()`: the text must decode to an object or a list
/// (PHP arrays); the error is PHP's message when it doesn't.
pub fn decode(text: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => Ok(value),
        Ok(_) => Err("JSON decoding failed: expected an object or a list.".to_owned()),
        Err(error) => Err(format!("JSON decoding failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn formats_like_phutil_json() {
        let value = json!({
            "a": 1,
            "b/c": ["x", "é"],
            "d": {},
            "e": {"0": "zero", "1": "one"},
            "f": {"k": true, "l": null},
            "g": [],
        });
        assert_eq!(
            encode_formatted(value.as_object().unwrap()),
            "{\n  \"a\": 1,\n  \"b/c\": [\n    \"x\",\n    \"\\u00e9\"\n  ],\n  \"d\": [],\n  \
             \"e\": [\n    \"zero\",\n    \"one\"\n  ],\n  \"f\": {\n    \"k\": true,\n    \
             \"l\": null\n  },\n  \"g\": []\n}\n"
        );
        assert_eq!(encode_formatted(&Map::new()), "{}\n");
    }

    #[test]
    fn decodes_only_arrays() {
        assert!(decode("{\"a\": 1}").is_ok());
        assert!(decode("[1]").is_ok());
        assert!(decode("1").is_err());
        assert!(decode("{").is_err());
    }
}
