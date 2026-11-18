//! PHP's semantics for the values Conduit and the configuration carry, which
//! the Rust code must reproduce to answer like PHP.

use std::borrow::Cow;
use std::fmt::Write as _;

use serde_json::Value;

/// Whether `if ($value)` takes the branch.
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
        Value::String(text) => is_truthy_str(text),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

/// Whether `if ($text)` takes the branch for a string: only `""` and `"0"`
/// are falsy.
pub fn is_truthy_str(text: &str) -> bool {
    !text.is_empty() && text != "0"
}

/// What `%s` prints for a value.
pub fn string_cast(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null | Value::Bool(false) => Cow::Borrowed(""),
        Value::Bool(true) => Cow::Borrowed("1"),
        Value::String(text) => Cow::Borrowed(text),
        Value::Array(_) | Value::Object(_) => Cow::Borrowed("Array"),
        Value::Number(number) => Cow::Owned(number.to_string()),
    }
}

/// `json_encode()` with its default flags: like serde's encoding, but `/`
/// is escaped and characters beyond ASCII are `\u` escapes (in UTF-16
/// halves for characters beyond the BMP).
pub fn json_encode(value: &Value) -> String {
    let plain = value.to_string();
    let mut output = String::with_capacity(plain.len());
    for character in plain.chars() {
        match character {
            '/' => output.push_str("\\/"),
            character if character.is_ascii() => output.push(character),
            character => {
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    let _ = write!(output, "\\u{unit:04x}");
                }
            }
        }
    }
    output
}

/// A JSON value as `json_encode(json_decode($json, true))` gives it back: PHP decodes
/// objects to arrays, so an empty object or one keyed `0..n` is a list.
pub fn round_trip(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let is_list = map
                .keys()
                .enumerate()
                .all(|(index, key)| *key == index.to_string());
            if is_list {
                Value::Array(map.values().map(round_trip).collect())
            } else {
                Value::Object(
                    map.iter()
                        .map(|(key, value)| (key.clone(), round_trip(value)))
                        .collect(),
                )
            }
        }
        Value::Array(list) => Value::Array(list.iter().map(round_trip).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    #[test]
    fn json_encodes_like_php() {
        assert_eq!(
            json_encode(&json!({"a/b": ["é", "😀", null], "n": 1})),
            r#"{"a\/b":["\u00e9","\ud83d\ude00",null],"n":1}"#
        );
    }

    use super::*;

    #[rstest]
    #[case(json!(null), false)]
    #[case(json!(false), false)]
    #[case(json!(0), false)]
    #[case(json!(0.0), false)]
    #[case(json!(""), false)]
    #[case(json!("0"), false)]
    #[case(json!([]), false)]
    #[case(json!({}), false)]
    #[case(json!(true), true)]
    #[case(json!(2), true)]
    #[case(json!("false"), true)]
    #[case(json!([0]), true)]
    fn truthiness_matches_php(#[case] value: Value, #[case] expected: bool) {
        assert_eq!(is_truthy(&value), expected);
    }

    #[rstest]
    #[case(json!(null), "")]
    #[case(json!(false), "")]
    #[case(json!(true), "1")]
    #[case(json!("abc"), "abc")]
    #[case(json!(12), "12")]
    #[case(json!([1]), "Array")]
    fn string_casts_match_php(#[case] value: Value, #[case] expected: &str) {
        assert_eq!(string_cast(&value), expected);
    }

    #[test]
    fn php_values_turn_objects_into_lists() {
        assert_eq!(round_trip(&json!({})), json!([]));
        assert_eq!(round_trip(&json!({"0": "a", "1": "b"})), json!(["a", "b"]));
        assert_eq!(round_trip(&json!({"x": {}})), json!({"x": []}));
    }
}
