//! Codec for PHP's `serialize()` format.
//!
//! Columns using `LiskDAO::SERIALIZATION_PHP` (markup cache, Herald
//! transcripts) and some cache keys (`PhabricatorUserProfileImageCacheType`)
//! use this format. Objects and references are rejected: Phabricator never
//! stores them in columns the Rust services read.

use std::fmt::Write as _;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Int(i64),
    Str(Vec<u8>),
}

impl Key {
    /// PHP converts string keys which look like canonical integers ("5",
    /// "-3", but not "05" or "1.5") to integer keys.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match canonical_integer(&bytes) {
            Some(number) => Self::Int(number),
            None => Self::Str(bytes),
        }
    }
}

impl From<&str> for Key {
    fn from(value: &str) -> Self {
        Self::from_bytes(value.as_bytes().to_vec())
    }
}

impl From<i64> for Key {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Array(Vec<(Key, Self)>),
}

impl Value {
    pub fn str(value: &str) -> Self {
        Self::Str(value.as_bytes().to_vec())
    }

    /// A PHP list (`array('a', 'b')`).
    pub fn list(values: impl IntoIterator<Item = Self>) -> Self {
        Self::Array(
            values
                .into_iter()
                .zip(0..)
                .map(|(value, index)| (Key::Int(index), value))
                .collect(),
        )
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Str(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn get(&self, key: &Key) -> Option<&Self> {
        match self {
            Self::Array(entries) => entries
                .iter()
                .find(|(entry_key, _)| entry_key == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

impl From<Option<&str>> for Value {
    fn from(value: Option<&str>) -> Self {
        value.map_or(Self::Null, Self::str)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UnserializeError {
    #[error("unexpected end of input at offset {0}")]
    UnexpectedEnd(usize),
    #[error("unexpected byte at offset {0}")]
    UnexpectedByte(usize),
    #[error("invalid number at offset {0}")]
    InvalidNumber(usize),
    #[error("unsupported type '{kind}' at offset {offset}")]
    Unsupported { kind: char, offset: usize },
    #[error("array keys must be integers or strings (offset {0})")]
    InvalidKey(usize),
    #[error("nesting deeper than {MAX_DEPTH} levels")]
    TooDeep,
}

const MAX_DEPTH: usize = 512;

fn canonical_integer(bytes: &[u8]) -> Option<i64> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    let is_canonical = match digits {
        [] => false,
        [b'0'] => bytes.len() == 1,
        [first, ..] => *first != b'0' && digits.iter().all(u8::is_ascii_digit),
    };
    if !is_canonical {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

pub fn serialize(value: &Value) -> Vec<u8> {
    let mut output = Vec::new();
    write_value(&mut output, value);
    output
}

fn write_value(output: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => output.extend_from_slice(b"N;"),
        Value::Bool(flag) => {
            output.extend_from_slice(if *flag { b"b:1;" } else { b"b:0;" });
        }
        Value::Int(number) => output.extend_from_slice(format!("i:{number};").as_bytes()),
        Value::Float(number) => {
            output.extend_from_slice(format!("d:{};", format_float(*number)).as_bytes());
        }
        Value::Str(bytes) => write_string(output, bytes),
        Value::Array(entries) => {
            output.extend_from_slice(format!("a:{}:{{", entries.len()).as_bytes());
            for (key, entry) in entries {
                match key {
                    Key::Int(number) => {
                        output.extend_from_slice(format!("i:{number};").as_bytes());
                    }
                    Key::Str(bytes) => write_string(output, bytes),
                }
                write_value(output, entry);
            }
            output.push(b'}');
        }
    }
}

fn write_string(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(format!("s:{}:\"", bytes.len()).as_bytes());
    output.extend_from_slice(bytes);
    output.extend_from_slice(b"\";");
}

/// Formats a float like PHP does with `serialize_precision = -1`: the
/// shortest representation that round-trips, in scientific notation when the
/// decimal exponent is below -4 or at least 17.
fn format_float(number: f64) -> String {
    if number.is_nan() {
        return "NAN".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 { "INF" } else { "-INF" }.to_owned();
    }
    if number == 0.0 {
        return if number.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }

    // `{:e}` gives the shortest round-trip digits, e.g. "-1.2345e20".
    let scientific = format!("{number:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();

    let mut result = String::from(sign);
    if !(-4..17).contains(&exponent) {
        result.push_str(&digits[..1]);
        result.push('.');
        result.push_str(if digits.len() > 1 { &digits[1..] } else { "0" });
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        let _ = write!(result, "E{exponent_sign}{}", exponent.unsigned_abs());
    } else if exponent < 0 {
        result.push_str("0.");
        result.push_str(&"0".repeat(exponent.unsigned_abs() as usize - 1));
        result.push_str(&digits);
    } else {
        let integer_length = exponent.unsigned_abs() as usize + 1;
        if digits.len() <= integer_length {
            result.push_str(&digits);
            result.push_str(&"0".repeat(integer_length - digits.len()));
        } else {
            result.push_str(&digits[..integer_length]);
            result.push('.');
            result.push_str(&digits[integer_length..]);
        }
    }
    result
}

/// Equivalent of `unserialize()`. Like PHP, data after the first complete
/// value is ignored.
pub fn unserialize(input: &[u8]) -> Result<Value, UnserializeError> {
    let mut parser = Parser { input, offset: 0 };
    parser.value(0)
}

struct Parser<'input> {
    input: &'input [u8],
    offset: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Result<u8, UnserializeError> {
        self.input
            .get(self.offset)
            .copied()
            .ok_or(UnserializeError::UnexpectedEnd(self.offset))
    }

    fn expect(&mut self, byte: u8) -> Result<(), UnserializeError> {
        if self.peek()? != byte {
            return Err(UnserializeError::UnexpectedByte(self.offset));
        }
        self.offset += 1;
        Ok(())
    }

    /// Bytes up to (not including) `terminator`, which is consumed.
    fn until(&mut self, terminator: u8) -> Result<&[u8], UnserializeError> {
        let start = self.offset;
        let length = self.input[start..]
            .iter()
            .position(|&byte| byte == terminator)
            .ok_or(UnserializeError::UnexpectedEnd(self.input.len()))?;
        self.offset = start + length + 1;
        Ok(&self.input[start..start + length])
    }

    fn integer(&mut self, terminator: u8) -> Result<i64, UnserializeError> {
        let start = self.offset;
        let text = self.until(terminator)?;
        let digits = text
            .strip_prefix(b"+")
            .or_else(|| text.strip_prefix(b"-"))
            .unwrap_or(text);
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(UnserializeError::InvalidNumber(start));
        }
        std::str::from_utf8(text)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or(UnserializeError::InvalidNumber(start))
    }

    fn length(&mut self) -> Result<usize, UnserializeError> {
        let start = self.offset;
        let text = self.until(b':')?;
        if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
            return Err(UnserializeError::InvalidNumber(start));
        }
        std::str::from_utf8(text)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or(UnserializeError::InvalidNumber(start))
    }

    fn string_body(&mut self) -> Result<Vec<u8>, UnserializeError> {
        let length = self.length()?;
        self.expect(b'"')?;
        let end = self
            .offset
            .checked_add(length)
            .filter(|end| *end <= self.input.len())
            .ok_or(UnserializeError::UnexpectedEnd(self.input.len()))?;
        let bytes = self.input[self.offset..end].to_vec();
        self.offset = end;
        self.expect(b'"')?;
        self.expect(b';')?;
        Ok(bytes)
    }

    fn value(&mut self, depth: usize) -> Result<Value, UnserializeError> {
        if depth > MAX_DEPTH {
            return Err(UnserializeError::TooDeep);
        }
        let kind_offset = self.offset;
        let kind = self.peek()?;
        self.offset += 1;
        if kind == b'N' {
            self.expect(b';')?;
            return Ok(Value::Null);
        }
        self.expect(b':')?;
        match kind {
            b'b' => match self.until(b';')? {
                b"0" => Ok(Value::Bool(false)),
                b"1" => Ok(Value::Bool(true)),
                _ => Err(UnserializeError::InvalidNumber(kind_offset + 2)),
            },
            b'i' => Ok(Value::Int(self.integer(b';')?)),
            b'd' => {
                let start = self.offset;
                let text = self.until(b';')?;
                parse_float(text)
                    .map(Value::Float)
                    .ok_or(UnserializeError::InvalidNumber(start))
            }
            b's' => Ok(Value::Str(self.string_body()?)),
            b'a' => self.array(depth),
            other => Err(UnserializeError::Unsupported {
                kind: char::from(other),
                offset: kind_offset,
            }),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, UnserializeError> {
        let count = self.length()?;
        self.expect(b'{')?;
        // Each entry takes at least 8 bytes ("i:0;N;"), which bounds how much
        // we pre-allocate for hostile counts.
        let mut entries = Vec::with_capacity(count.min(self.input.len() / 6));
        for _ in 0..count {
            let key_offset = self.offset;
            let key = match self.value(depth + 1)? {
                Value::Int(number) => Key::Int(number),
                Value::Str(bytes) => Key::from_bytes(bytes),
                _ => return Err(UnserializeError::InvalidKey(key_offset)),
            };
            let value = self.value(depth + 1)?;
            // A repeated key overwrites the earlier entry in place.
            match entries
                .iter_mut()
                .find(|(existing, _): &&mut (Key, Value)| *existing == key)
            {
                Some(entry) => entry.1 = value,
                None => entries.push((key, value)),
            }
        }
        self.expect(b'}')?;
        Ok(Value::Array(entries))
    }
}

fn parse_float(text: &[u8]) -> Option<f64> {
    match text {
        b"INF" => Some(f64::INFINITY),
        b"-INF" => Some(f64::NEG_INFINITY),
        b"NAN" => Some(f64::NAN),
        _ => {
            let text = std::str::from_utf8(text).ok()?;
            let valid = !text.is_empty()
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte));
            if valid { text.parse().ok() } else { None }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rstest::rstest;

    use super::*;

    // Expected output from PHP 8.4's serialize().
    #[rstest]
    #[case(0.1, "d:0.1;")]
    #[case(1.0, "d:1;")]
    #[case(-0.0, "d:-0;")]
    #[case(1e25, "d:1.0E+25;")]
    #[case(1.5e-7, "d:1.5E-7;")]
    #[case(123_456_789_012_345.0, "d:123456789012345;")]
    #[case(1e16, "d:10000000000000000;")]
    #[case(1e17, "d:1.0E+17;")]
    #[case(1.5e16, "d:15000000000000000;")]
    #[case(0.0001, "d:0.0001;")]
    #[case(0.00012, "d:0.00012;")]
    #[case(0.00001, "d:1.0E-5;")]
    #[case(-2.5, "d:-2.5;")]
    #[case(1.2345e20, "d:1.2345E+20;")]
    #[case(5e-324, "d:5.0E-324;")]
    #[case(f64::MAX, "d:1.7976931348623157E+308;")]
    #[case(0.300_000_000_000_000_04, "d:0.30000000000000004;")]
    #[case(f64::INFINITY, "d:INF;")]
    #[case(f64::NEG_INFINITY, "d:-INF;")]
    #[case(f64::NAN, "d:NAN;")]
    fn floats_match_php(#[case] number: f64, #[case] expected: &str) {
        assert_eq!(serialize(&Value::Float(number)), expected.as_bytes());
    }

    #[test]
    fn scalars_and_arrays_match_php() {
        assert_eq!(serialize(&Value::Int(i64::MIN)), b"i:-9223372036854775808;");
        assert_eq!(serialize(&Value::Bool(true)), b"b:1;");
        assert_eq!(serialize(&Value::Null), b"N;");
        assert_eq!(serialize(&Value::str("é\"x")), "s:4:\"é\"x\";".as_bytes());
        let array = Value::Array(vec![
            (Key::Int(1), Value::str("a")),
            (Key::from("k"), Value::list([Value::Bool(true)])),
        ]);
        assert_eq!(
            serialize(&array),
            b"a:2:{i:1;s:1:\"a\";s:1:\"k\";a:1:{i:0;b:1;}}"
        );
    }

    #[test]
    fn profile_image_cache_version_input_matches_php() {
        // serialize(array('http://phabricator.test/', null, null))
        let parts = Value::list([
            Value::str("http://phabricator.test/"),
            Value::Null,
            Value::Null,
        ]);
        assert_eq!(
            serialize(&parts),
            b"a:3:{i:0;s:24:\"http://phabricator.test/\";i:1;N;i:2;N;}"
        );
    }

    #[rstest]
    #[case("10", Key::Int(10))]
    #[case("-5", Key::Int(-5))]
    #[case("0", Key::Int(0))]
    #[case("07", Key::Str(b"07".to_vec()))]
    #[case("-0", Key::Str(b"-0".to_vec()))]
    #[case("1.5", Key::Str(b"1.5".to_vec()))]
    #[case("99999999999999999999", Key::Str(b"99999999999999999999".to_vec()))]
    fn numeric_string_keys_become_integers(#[case] key: &str, #[case] expected: Key) {
        assert_eq!(Key::from(key), expected);
    }

    #[rstest]
    #[case("i:+5;", Value::Int(5))]
    #[case("i:-0;", Value::Int(0))]
    #[case("d:1e3;", Value::Float(1000.0))]
    #[case("a:1:{i:0;i:1;};trailing", Value::list([Value::Int(1)]))]
    #[case("a:1:{s:1:\"5\";i:1;}", Value::Array(vec![(Key::Int(5), Value::Int(1))]))]
    #[case(
        "a:2:{i:0;i:1;i:0;i:2;}",
        Value::Array(vec![(Key::Int(0), Value::Int(2))])
    )]
    fn unserializes_like_php(#[case] input: &str, #[case] expected: Value) {
        assert_eq!(unserialize(input.as_bytes()).unwrap(), expected);
    }

    #[rstest]
    #[case("b:2;")]
    #[case("s:1:\"ab\";")]
    #[case("s:5:\"ab\";")]
    #[case("i:;")]
    #[case("i:1")]
    #[case("O:8:\"stdClass\":0:{}")]
    #[case("r:1;")]
    #[case("a:1:{d:1.5;i:1;}")]
    #[case("a:1:{i:0;i:1;")]
    #[case("")]
    fn rejects_malformed_input(#[case] input: &str) {
        assert!(unserialize(input.as_bytes()).is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        let input = "a:1:{i:0;".repeat(MAX_DEPTH + 2);
        assert_eq!(
            unserialize(input.as_bytes()),
            Err(UnserializeError::TooDeep)
        );
    }

    fn values() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::Int),
            any::<f64>()
                .prop_filter("NAN never compares equal", |number| !number.is_nan())
                .prop_map(Value::Float),
            proptest::collection::vec(any::<u8>(), 0..16).prop_map(Value::Str),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            proptest::collection::vec(
                (
                    prop_oneof![
                        any::<i64>().prop_map(Key::Int),
                        "[a-z]{0,6}".prop_map(|key| Key::from(key.as_str())),
                    ],
                    inner,
                ),
                0..8,
            )
            .prop_map(|entries| {
                let mut unique: Vec<(Key, Value)> = Vec::new();
                for (key, value) in entries {
                    if !unique.iter().any(|(existing, _)| *existing == key) {
                        unique.push((key, value));
                    }
                }
                Value::Array(unique)
            })
        })
    }

    proptest! {
        #[test]
        fn round_trips(value in values()) {
            prop_assert_eq!(unserialize(&serialize(&value)).unwrap(), value);
        }

        #[test]
        fn never_panics_on_arbitrary_input(input in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = unserialize(&input);
        }

        #[test]
        fn never_panics_on_mutated_serialized_data(
            value in values(),
            position in any::<prop::sample::Index>(),
            byte in any::<u8>(),
        ) {
            let mut data = serialize(&value);
            if !data.is_empty() {
                let index = position.index(data.len());
                data[index] = byte;
            }
            let _ = unserialize(&data);
        }
    }
}
