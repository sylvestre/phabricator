//! The PHP semantics the Git query methods depend on: arrays, whose
//! string keys which look like integers become integers and which encode
//! as JSON lists when their keys are `0..n`, and the casts, slices and
//! splits their code applies to parameters and command output.

use indexmap::IndexMap;
use phab_conduit::MethodError;
use phab_core::php;
use serde_json::{Map, Value};

/// A PHP array key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// An integer key.
    Int(i64),
    /// A string key.
    Str(String),
}

impl Key {
    /// The key PHP makes of a string: canonical integers ("5", "-3", not
    /// "05" or "+1") become integers.
    pub fn from_text(text: &str) -> Self {
        let digits = text.strip_prefix('-').unwrap_or(text);
        let canonical = !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (digits == "0" && digits.len() == text.len() || !digits.starts_with('0'));
        match text.parse() {
            Ok(number) if canonical => Self::Int(number),
            _ => Self::Str(text.to_owned()),
        }
    }

    /// The key as JSON writes it, and as PHP casts it to a string.
    pub fn text(&self) -> String {
        match self {
            Self::Int(number) => number.to_string(),
            Self::Str(text) => text.clone(),
        }
    }
}

/// An ordered PHP array.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Array {
    entries: IndexMap<Key, Value>,
}

impl Array {
    /// An empty array.
    pub fn new() -> Self {
        Self::default()
    }

    /// `$array[$key] = $value`: an existing key keeps its position.
    pub fn set(&mut self, key: Key, value: Value) {
        self.entries.insert(key, value);
    }

    /// `$array[] = $value`.
    pub fn push(&mut self, value: Value) {
        let next = self
            .entries
            .keys()
            .filter_map(|key| match key {
                Key::Int(number) => Some(number.saturating_add(1)),
                Key::Str(_) => None,
            })
            .max()
            .unwrap_or(0)
            .max(0);
        self.entries.insert(Key::Int(next), value);
    }

    /// `$array[$key]`.
    pub fn get(&self, key: &Key) -> Option<&Value> {
        self.entries.get(key)
    }

    /// `$array[$key]`, for changing it.
    pub fn get_mut(&mut self, key: &Key) -> Option<&mut Value> {
        self.entries.get_mut(key)
    }

    /// `unset($array[$key])`.
    pub fn remove(&mut self, key: &Key) {
        self.entries.shift_remove(key);
    }

    /// Whether the array is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entries in order.
    pub fn iter(&self) -> impl Iterator<Item = (&Key, &Value)> {
        self.entries.iter()
    }

    /// Keeps the entries for which `keep` is true.
    pub fn retain(&mut self, mut keep: impl FnMut(&Key, &Value) -> bool) {
        self.entries.retain(|key, value| keep(key, value));
    }

    /// `json_encode()`: a list when the keys are `0..n` in order, an
    /// object otherwise.
    pub fn into_json(self) -> Value {
        let is_list =
            self.entries.keys().enumerate().all(|(index, key)| {
                i64::try_from(index).is_ok_and(|index| *key == Key::Int(index))
            });
        if is_list {
            Value::Array(self.entries.into_values().collect())
        } else {
            Value::Object(
                self.entries
                    .into_iter()
                    .map(|(key, value)| (key.text(), value))
                    .collect::<Map<String, Value>>(),
            )
        }
    }
}

impl FromIterator<(Key, Value)> for Array {
    fn from_iter<I: IntoIterator<Item = (Key, Value)>>(entries: I) -> Self {
        let mut array = Self::new();
        for (key, value) in entries {
            array.set(key, value);
        }
        array
    }
}

/// What `%s` of `csprintf()` makes of a parameter: strings, integers and
/// booleans; `null` is the empty string. Other values are left to PHP.
pub fn shell_string(value: Option<&Value>) -> Result<String, MethodError> {
    match value {
        None | Some(Value::Null | Value::Bool(_) | Value::String(_)) => {
            Ok(php::string_cast(value.unwrap_or(&Value::Null)).into_owned())
        }
        Some(Value::Number(number)) if !number.is_f64() => Ok(number.to_string()),
        Some(_) => Err(MethodError::Unsupported("an unusual parameter type")),
    }
}

/// A parameter which must be a string (or absent); other values make PHP
/// warn or throw, which is left to PHP.
pub fn optional_string(value: Option<&Value>) -> Result<Option<String>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(MethodError::Unsupported(
            "a parameter which is not a string",
        )),
    }
}

/// A parameter which must be a string.
pub fn string(value: Option<&Value>) -> Result<String, MethodError> {
    optional_string(value)?.ok_or(MethodError::Unsupported("a missing string parameter"))
}

/// `phutil_nonempty_string()`, which throws on values which are not
/// strings.
pub fn nonempty_string(value: Option<&Value>) -> Result<Option<String>, MethodError> {
    Ok(optional_string(value)?.filter(|text| !text.is_empty()))
}

/// `(int)$value`. Strings which don't plainly start with an integer
/// (`"1e3"`, `" 1.5"`) are left to PHP.
pub fn int_cast(value: Option<&Value>) -> Result<i64, MethodError> {
    let unusual = MethodError::Unsupported("an unusual integer parameter");
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Ok(0),
        Some(Value::Bool(true)) => Ok(1),
        Some(Value::Number(number)) => number.as_i64().ok_or(unusual),
        Some(Value::String(text)) => leading_int(text).ok_or(unusual),
        Some(_) => Err(unusual),
    }
}

/// The integer a numeric string starts with; `Some(0)` for a string
/// which doesn't start like a number, `None` where PHP's rules for
/// floats or overflows would apply.
fn leading_int(text: &str) -> Option<i64> {
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1, &trimmed[1..]),
        Some(b'+') => (1, &trimmed[1..]),
        _ => (1, trimmed),
    };
    let length = digits.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &digits[length..];
    if rest.starts_with(['.', 'e', 'E']) {
        return None;
    }
    if length == 0 {
        // "abc" is 0, "-x" too; "-.5" was caught above.
        return Some(0);
    }
    digits[..length]
        .parse::<i64>()
        .ok()
        .map(|number| sign * number)
}

/// A parameter used as an integer by `array_slice()` or arithmetic:
/// integers, integer strings and `null` (as `default`); other values make
/// PHP throw or warn, and are left to PHP.
pub fn int_param(value: Option<&Value>, default: i64) -> Result<i64, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => number
            .as_i64()
            .ok_or(MethodError::Unsupported("an unusual integer parameter")),
        Some(Value::String(text)) => {
            let digits = text.strip_prefix('-').unwrap_or(text);
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                text.parse()
                    .map_err(|_| MethodError::Unsupported("a huge integer parameter"))
            } else {
                Err(MethodError::Unsupported("an unusual integer parameter"))
            }
        }
        Some(_) => Err(MethodError::Unsupported("an unusual integer parameter")),
    }
}

/// The range `array_slice($array, $offset, $length)` keeps of an array
/// of `count` entries.
pub fn slice(count: usize, offset: i64, length: Option<i64>) -> std::ops::Range<usize> {
    let count = i64::try_from(count).unwrap_or(i64::MAX);
    let start = if offset < 0 {
        (count + offset).max(0)
    } else {
        offset.min(count)
    };
    let end = match length {
        None => count,
        Some(length) if length < 0 => (count + length).max(start),
        Some(length) => start.saturating_add(length).min(count),
    };
    let start = usize::try_from(start).unwrap_or(0);
    let end = usize::try_from(end).unwrap_or(0).max(start);
    start..end
}

/// The characters `\s` matches without the `u` flag.
pub fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `preg_split('/\s+/', $text, $limit)`, `$limit` 0 meaning no limit.
pub fn split_whitespace(text: &str, limit: usize) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if limit > 0 && pieces.len() + 1 == limit {
            break;
        }
        if is_space(bytes[index]) {
            let end = index;
            while index < bytes.len() && is_space(bytes[index]) {
                index += 1;
            }
            pieces.push(&text[start..end]);
            start = index;
        } else {
            index += 1;
        }
    }
    pieces.push(&text[start..]);
    pieces
}

/// `trim()` with its default characters.
pub fn trim(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
}

/// `rtrim()` with its default characters.
pub fn rtrim(text: &str) -> &str {
    text.trim_end_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
}

/// Command output as text; output which is not UTF-8 can't be encoded
/// as PHP would, and is left to PHP.
pub fn text(bytes: Vec<u8>) -> Result<String, MethodError> {
    String::from_utf8(bytes).map_err(|_| MethodError::Unsupported("output which is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn makes_keys_like_php() {
        assert_eq!(Key::from_text("12"), Key::Int(12));
        assert_eq!(Key::from_text("-3"), Key::Int(-3));
        assert_eq!(Key::from_text("0"), Key::Int(0));
        for text in ["012", "-0", "+1", "1.5", "", "99999999999999999999", "a"] {
            assert_eq!(Key::from_text(text), Key::Str(text.to_owned()), "{text}");
        }
    }

    #[test]
    fn encodes_lists_and_maps() {
        let mut array = Array::new();
        array.push(json!("a"));
        array.push(json!("b"));
        assert_eq!(array.clone().into_json(), json!(["a", "b"]));
        array.remove(&Key::Int(0));
        assert_eq!(array.clone().into_json(), json!({"1": "b"}));
        array.set(Key::from_text("x"), json!(1));
        array.set(Key::Int(1), json!("c"));
        assert_eq!(array.into_json(), json!({"1": "c", "x": 1}));
        assert_eq!(Array::new().into_json(), json!([]));
    }

    #[test]
    fn slices_like_php() {
        assert_eq!(slice(5, 0, None), 0..5);
        assert_eq!(slice(5, 2, Some(2)), 2..4);
        assert_eq!(slice(5, 7, None), 5..5);
        assert_eq!(slice(5, -2, None), 3..5);
        assert_eq!(slice(5, 1, Some(-1)), 1..4);
        assert_eq!(slice(5, 4, Some(-3)), 4..4);
    }

    #[test]
    fn casts_and_splits_like_php() {
        assert_eq!(int_cast(Some(&json!("12abc"))), Ok(12));
        assert_eq!(int_cast(Some(&json!(" -3"))), Ok(-3));
        assert_eq!(int_cast(Some(&json!("abc"))), Ok(0));
        assert!(int_cast(Some(&json!("1e3"))).is_err());
        assert_eq!(split_whitespace("a  b\tc d", 3), ["a", "b", "c d"]);
        assert_eq!(split_whitespace("", 0), [""]);
        assert_eq!(split_whitespace("a b", 0), ["a", "b"]);
    }
}
