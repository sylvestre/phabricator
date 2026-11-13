//! Compares the answers PHP and Rust give to the same Conduit call.
//!
//! The comparison is structural: object key order doesn't matter and
//! numbers are equal when their values are (`1` and `1.0`). Everything
//! else, including the difference between a string `"1"` and a number `1`
//! or between `[]` and `{}`, is reported, because clients see it.

use serde_json::Value;

/// Asks nginx to send a request to PHP even when the Rust service handles
/// its route. The shadow mode and phab-replay set it on their PHP calls.
pub const SKIP_RUST_HEADER: &str = "x-phab-skip-rust";

#[derive(Debug, Clone, PartialEq)]
/// A value which differs between the PHP and the Rust answer.
pub struct Difference {
    /// JSON Pointer of the differing value, e.g. `/result/data/0/epoch`.
    pub path: String,
    /// The PHP value, `None` if PHP has none.
    pub php: Option<Value>,
    /// The Rust value, `None` if Rust has none.
    pub rust: Option<Value>,
}

/// Paths whose values legitimately differ between the stacks.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    ignored: Vec<String>,
}

impl Rules {
    /// The rules for `method`.
    pub fn for_method(method: &str) -> Self {
        let ignored: &[&str] = match method {
            // The host name of whichever server answered.
            "conduit.ping" => &["/result"],
            _ => &[],
        };
        Self {
            ignored: ignored.iter().map(|path| (*path).to_owned()).collect(),
        }
    }

    /// Also ignores `path`, a JSON Pointer.
    #[must_use]
    pub fn ignoring(mut self, path: &str) -> Self {
        self.ignored.push(path.to_owned());
        self
    }

    fn is_ignored(&self, path: &str) -> bool {
        self.ignored.iter().any(|ignored| ignored == path)
    }
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Differences between the PHP and the Rust answer, in document order.
pub fn compare(php: &Value, rust: &Value, rules: &Rules) -> Vec<Difference> {
    let mut differences = Vec::new();
    walk("", Some(php), Some(rust), rules, &mut differences);
    differences
}

fn walk(
    path: &str,
    php: Option<&Value>,
    rust: Option<&Value>,
    rules: &Rules,
    differences: &mut Vec<Difference>,
) {
    if rules.is_ignored(path) {
        return;
    }
    let mismatch = |differences: &mut Vec<Difference>| {
        differences.push(Difference {
            path: path.to_owned(),
            php: php.cloned(),
            rust: rust.cloned(),
        });
    };
    match (php, rust) {
        (Some(Value::Object(left)), Some(Value::Object(right))) => {
            for (key, value) in left {
                walk(
                    &format!("{path}/{}", escape(key)),
                    Some(value),
                    right.get(key),
                    rules,
                    differences,
                );
            }
            for (key, value) in right {
                if !left.contains_key(key) {
                    walk(
                        &format!("{path}/{}", escape(key)),
                        None,
                        Some(value),
                        rules,
                        differences,
                    );
                }
            }
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) => {
            for index in 0..left.len().max(right.len()) {
                walk(
                    &format!("{path}/{index}"),
                    left.get(index),
                    right.get(index),
                    rules,
                    differences,
                );
            }
        }
        (Some(Value::Number(left)), Some(Value::Number(right))) => {
            if left != right && left.as_f64() != right.as_f64() {
                mismatch(differences);
            }
        }
        (left, right) => {
            if left != right {
                mismatch(differences);
            }
        }
    }
}

/// A one-line description of the differences, for logs.
pub fn summarize(differences: &[Difference]) -> String {
    let render = |value: &Option<Value>| {
        value
            .as_ref()
            .map_or_else(|| "(missing)".to_owned(), Value::to_string)
    };
    differences
        .iter()
        .map(|difference| {
            format!(
                "{}: php={} rust={}",
                if difference.path.is_empty() {
                    "/"
                } else {
                    &difference.path
                },
                render(&difference.php),
                render(&difference.rust)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn paths(php: &Value, rust: &Value, rules: &Rules) -> Vec<String> {
        compare(php, rust, rules)
            .into_iter()
            .map(|difference| difference.path)
            .collect()
    }

    #[rstest]
    #[case(json!({"a": 1, "b": [1, 2]}), json!({"b": [1, 2], "a": 1}))]
    #[case(json!({"n": 1}), json!({"n": 1.0}))]
    #[case(json!(null), json!(null))]
    fn equal_answers(#[case] php: Value, #[case] rust: Value) {
        assert_eq!(compare(&php, &rust, &Rules::default()), vec![]);
    }

    #[rstest]
    #[case(json!({"id": "12"}), json!({"id": 12}), &["/id"])]
    #[case(json!({"data": []}), json!({"data": {}}), &["/data"])]
    #[case(json!({"a": 1}), json!({"b": 1}), &["/a", "/b"])]
    #[case(json!([1, 2, 3]), json!([1, 5]), &["/1", "/2"])]
    #[case(json!({"a/b": {"c~d": 1}}), json!({"a/b": {"c~d": 2}}), &["/a~1b/c~0d"])]
    #[case(json!(1), json!("1"), &[""])]
    fn reports_differences(#[case] php: Value, #[case] rust: Value, #[case] expected: &[&str]) {
        assert_eq!(paths(&php, &rust, &Rules::default()), expected);
    }

    #[test]
    fn ignores_configured_paths() {
        let php = json!({"result": "web1", "error_code": null});
        let rust = json!({"result": "rust1", "error_code": null});
        assert_eq!(
            compare(&php, &rust, &Rules::for_method("conduit.ping")),
            vec![]
        );
        assert_eq!(paths(&php, &rust, &Rules::default()), ["/result"]);
        assert_eq!(
            compare(&php, &rust, &Rules::default().ignoring("/result")),
            vec![]
        );
    }

    #[test]
    fn summarizes() {
        let differences = compare(&json!({"a": 1}), &json!({}), &Rules::default());
        assert_eq!(summarize(&differences), "/a: php=1 rust=(missing)");
    }
}
