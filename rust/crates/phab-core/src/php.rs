//! PHP's semantics for the values Conduit and the configuration carry, which
//! the Rust code must reproduce to answer like PHP.

use std::borrow::Cow;

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

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

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
}
