//! JSON encoding byte-compatible with Conduit's PHP responses
//! (`json_encode()` defaults, then `<`/`>` escaped by
//! `AphrontResponse::encodeJSONForHTTPResponse()`).

use std::fmt::Write as _;

use phab_db::phpser::format_float;
use serde_json::Value;

pub fn encode(value: &Value) -> String {
    let mut output = String::new();
    write_value(&mut output, value);
    output
}

fn write_value(output: &mut String, value: &Value) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(flag) => output.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64().filter(|_| number.is_f64()) {
            Some(float) => output.push_str(&format_float(float).replace('E', "e")),
            None => {
                let _ = write!(output, "{number}");
            }
        },
        Value::String(text) => write_string(output, text),
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(output, item);
            }
            output.push(']');
        }
        Value::Object(entries) => {
            output.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_string(output, key);
                output.push(':');
                write_value(output, item);
            }
            output.push('}');
        }
    }
}

fn write_string(output: &mut String, text: &str) {
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '/' => output.push_str("\\/"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '<' => output.push_str("\\u003c"),
            '>' => output.push_str("\\u003e"),
            character if character.is_ascii_control() || !character.is_ascii() => {
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    let _ = write!(output, "\\u{unit:04x}");
                }
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn matches_php_json_encode() {
        // php -r 'echo json_encode(["a/b", "é😀", 1, 0.1, [], ["k"=>null], "\x01\"\\"]);'
        // with "<x>" escaped afterwards by encodeJSONForHTTPResponse().
        let value = json!(["a/b", "é😀", "<x>", 1, 0.1, [], {"k": null}, "\u{1}\"\\"]);
        assert_eq!(
            encode(&value),
            r#"["a\/b","\u00e9\ud83d\ude00","\u003cx\u003e",1,0.1,[],{"k":null},"\u0001\"\\"]"#
        );
    }

    #[test]
    fn floats_match_php() {
        assert_eq!(encode(&json!(1.0)), "1");
        assert_eq!(encode(&json!(-2.5)), "-2.5");
        assert_eq!(encode(&json!(1e20)), "1.0e+20");
    }
}
