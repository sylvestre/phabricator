//! The PHP string and comparison semantics mail code relies on.

use serde_json::Value;

/// The characters `trim()` strips by default.
const TRIM: &[char] = &[' ', '\t', '\n', '\r', '\0', '\x0b'];

/// `trim($text)`.
pub fn trim(text: &str) -> &str {
    text.trim_matches(TRIM)
}

/// `is_numeric($text)` for a string, as PHP 8 reads it: leading and
/// trailing whitespace allowed. Returns the value.
pub fn numeric(text: &str) -> Option<f64> {
    let whitespace = [' ', '\t', '\n', '\r', '\x0b', '\x0c'];
    let body = text.trim_matches(whitespace);
    let bytes = body.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let mut digits = index - start;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        digits += index - fraction;
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let mut exponent = index + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let exponent_digits = exponent;
        while bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            exponent += 1;
        }
        if exponent == exponent_digits {
            return None;
        }
        index = exponent;
    }
    if index != bytes.len() {
        return None;
    }
    body.parse().ok()
}

/// `$value == $target` for a decoded JSON value and a string constant,
/// with PHP 8's rules.
pub fn loose_eq_str(value: &Value, target: &str) -> bool {
    match value {
        Value::Null => target.is_empty(),
        Value::Bool(flag) => *flag == phab_core::php::is_truthy_str(target),
        Value::Number(number) => match numeric(target) {
            Some(target) => number.as_f64() == Some(target),
            None => number.to_string() == target,
        },
        #[expect(clippy::float_cmp, reason = "PHP compares numeric strings exactly")]
        Value::String(text) => match (numeric(text), numeric(target)) {
            (Some(left), Some(right)) => left == right,
            _ => text == target,
        },
        Value::Array(_) | Value::Object(_) => false,
    }
}

/// A value `phutil_nonempty_string()` throws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAString;

/// `phutil_nonempty_string($value)`.
pub fn nonempty_string(value: Option<&Value>) -> Result<Option<&str>, NotAString> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(NotAString),
    }
}

/// `phutil_string_cast($value)` for a scalar; `None` for arrays, which it
/// throws on.
pub fn string_cast(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::Array(_) | Value::Object(_)) => None,
        Some(value) => Some(phab_core::php::string_cast(value).into_owned()),
    }
}

/// `empty($text)` for a string.
pub fn is_empty_str(text: &str) -> bool {
    text.is_empty() || text == "0"
}

/// The values of a PHP array decoded from JSON, as `foreach` walks them;
/// `None` for a scalar, which `foreach` warns on.
pub fn values(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Array(list) => Some(list.iter().collect()),
        Value::Object(map) => Some(map.values().collect()),
        _ => None,
    }
}

/// A list of strings (PHIDs, tags, keys); `None` when an entry is not a
/// string, which Rust doesn't follow PHP through.
pub fn strings(value: &Value) -> Option<Vec<String>> {
    values(value)?
        .into_iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

/// `phid_get_type()`: the four characters after `PHID-`, or `XXXX`.
pub fn phid_type(phid: &str) -> &str {
    phid.strip_prefix("PHID-")
        .filter(|rest| rest.len() > 4 && rest.as_bytes().get(4) == Some(&b'-'))
        .and_then(|rest| rest.get(..4))
        .filter(|kind| !kind.contains('-'))
        .unwrap_or("XXXX")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case(json!("2"), "2", true)]
    #[case(json!(2), "2", true)]
    #[case(json!("2.0"), "2", true)]
    #[case(json!(" 2"), "2", true)]
    #[case(json!("2x"), "2", false)]
    #[case(json!(true), "2", true)]
    #[case(json!(false), "0", true)]
    #[case(Value::Null, "0", false)]
    #[case(json!("re"), "re", true)]
    #[case(json!(0), "re", false)]
    #[case(json!(true), "re", true)]
    #[case(json!(["re"]), "re", false)]
    fn compares_like_php(#[case] value: Value, #[case] target: &str, #[case] expected: bool) {
        assert_eq!(loose_eq_str(&value, target), expected);
    }

    #[test]
    fn reads_phid_types() {
        assert_eq!(phid_type("PHID-USER-abc"), "USER");
        assert_eq!(phid_type("PHID-PROJ-x"), "PROJ");
        assert_eq!(phid_type("PHID-AB-x"), "XXXX");
        assert_eq!(phid_type(""), "XXXX");
    }
}
