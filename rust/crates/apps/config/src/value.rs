//! Values from the command line, as `bin/config set` reads and checks them:
//! `PhabricatorConfigType::newValueFromCommandLineValue()`,
//! `validateStoredValue()` and the group's `validateOption()`.
//!
//! The types whose checks are PHP code of their own (`custom:*` types and
//! the JSON types of applications, like `cluster.mailers`) and the options
//! a group checks with code Rust doesn't have (`reviewhelper.*`) are
//! [`Error::Unsupported`]: `bin/config` must set them.

use serde_json::{Map, Value};

use crate::catalog::ConfigOption;
use crate::{Error, json};

fn usage(message: String) -> Error {
    Error::Usage(message)
}

/// `newValueFromCommandLineValue()` (or, for types without a class, the
/// workflow's `json_decode()`), then `validateStoredValue()`.
pub fn from_command_line(option: &ConfigOption, raw: &str) -> Result<Value, Error> {
    let key = &option.key;
    let kind = option.option_type.as_str();
    let value = match kind {
        "bool" => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => {
                return Err(usage(format!(
                    "Value for option \"{key}\" of type \"{kind}\" must be either \"true\" or \
                     \"false\"."
                )));
            }
        },
        "int" => {
            let digits = raw.strip_prefix('-').unwrap_or(raw);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(usage(format!(
                    "Value for option \"{key}\" must be an integer."
                )));
            }
            // `(int)` saturates; values that large aren't worth porting.
            let number: i64 = raw
                .parse()
                .map_err(|_| Error::Unsupported(format!("an integer out of range for {key}")))?;
            Value::from(number)
        }
        "string" | "class" | "enum" => Value::String(raw.to_owned()),
        "list<string>" | "list<regex>" => json::decode(raw).map_err(|_| {
            usage(format!(
                "Option \"{key}\" is of type \"{kind}\", but the value you provided is not a \
                 valid JSON list. When setting a list option from the command line, specify \
                 the value in JSON. You may need to quote the value for your shell (for \
                 example: '[\"a\", \"b\", ...]')."
            ))
        })?,
        "set" => set_from_command_line(option, raw)?,
        "wild" => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Null) if raw != "null" => return Err(wild_error(option)),
            Ok(value) => value,
            Err(_) => return Err(wild_error(option)),
        },
        other if other.starts_with("custom:") => {
            // The workflow wants a JSON array, then the custom type checks it.
            match serde_json::from_str::<Value>(raw) {
                Ok(Value::Array(_) | Value::Object(_)) => {}
                _ => {
                    return Err(usage(format!(
                        "Configuration key \"{key}\" is of type \"{other}\". Specify it in JSON."
                    )));
                }
            }
            return Err(Error::Unsupported(format!(
                "the checks of {other} (option {key})"
            )));
        }
        other => {
            return Err(Error::Unsupported(format!(
                "the checks of the {other} type (option {key})"
            )));
        }
    };
    validate_stored(option, &value)?;
    Ok(value)
}

fn wild_error(option: &ConfigOption) -> Error {
    usage(format!(
        "Value for option \"{}\" (of type \"{}\") must be specified in JSON, but input could \
         not be decoded. (Did you forget to quote a string?)",
        option.key, option.option_type
    ))
}

/// `PhabricatorSetConfigType::newValueFromCommandLineValue()`: a JSON list
/// of values, stored as a map of each value to `true`.
fn set_from_command_line(option: &ConfigOption, raw: &str) -> Result<Value, Error> {
    let (key, kind) = (&option.key, &option.option_type);
    let decoded = json::decode(raw).map_err(|_| {
        usage(format!(
            "Option \"{key}\" is of type \"{kind}\", but the value you provided is not a valid \
             JSON list: when providing a set from the command line, specify it as a list of \
             values in JSON. You may need to quote the value for your shell (for example: \
             '[\"a\", \"b\", ...]')."
        ))
    })?;
    let values: Vec<Value> = match decoded {
        Value::Array(values) => values,
        Value::Object(map) if map.is_empty() => Vec::new(),
        Value::Object(map) if json_is_list(&map) => map.into_iter().map(|(_, v)| v).collect(),
        Value::Object(_) => {
            return Err(usage(format!(
                "Option \"{key}\" is of type \"{kind}\", and should be specified on the command \
                 line as a JSON list of values. You may need to quote the value for your shell \
                 (for example: '[\"a\", \"b\", ...]')."
            )));
        }
        _ => unreachable!("json::decode() only returns arrays"),
    };
    let mut set = Map::new();
    for value in values {
        let member = match value {
            Value::String(text) => text,
            Value::Number(number) if number.is_i64() => number.to_string(),
            // `array_fill_keys()` warns on other keys.
            _ => {
                return Err(Error::Unsupported(format!(
                    "a set member which is not a string ({key})"
                )));
            }
        };
        set.insert(member, Value::Bool(true));
    }
    Ok(Value::Object(set))
}

fn json_is_list(map: &Map<String, Value>) -> bool {
    map.keys()
        .enumerate()
        .all(|(index, key)| *key == index.to_string())
}

/// `validateStoredValue()` of the option's type.
fn validate_stored(option: &ConfigOption, value: &Value) -> Result<(), Error> {
    let (key, kind) = (&option.key, option.option_type.as_str());
    match kind {
        // Sets come from `set_from_command_line()`, whose values are true.
        "bool" | "int" | "wild" | "string" | "set" => Ok(()),
        "enum" => {
            let text = value.as_str().unwrap_or_default();
            let map = option.enum_options.as_ref();
            if map.is_some_and(|map| map.contains_key(text)) {
                return Ok(());
            }
            let valid = map.map_or_else(String::new, |map| {
                map.keys().cloned().collect::<Vec<_>>().join(", ")
            });
            Err(usage(format!(
                "Option \"{key}\" is of type \"{kind}\", but the current value (\"{text}\") is \
                 not among the set of valid values: {valid}."
            )))
        }
        "class" => {
            let text = value.as_str().unwrap_or_default();
            let classes = option.classes.as_deref().unwrap_or_default();
            if classes.iter().any(|class| class == text) {
                return Ok(());
            }
            Err(usage(format!(
                "Option \"{key}\" is of type \"{kind}\", but the configured value is not the \
                 name of a known class. Valid selections are: {}.",
                classes.join(", ")
            )))
        }
        "list<string>" | "list<regex>" => {
            let items: Vec<(String, &Value)> = match value {
                Value::Array(list) => list
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (index.to_string(), item))
                    .collect(),
                Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
                _ => {
                    return Err(usage(format!(
                        "Option \"{key}\" is of type \"{kind}\", but the configured value is \
                         not a list."
                    )));
                }
            };
            for (expected, (index, item)) in items.iter().enumerate() {
                let Some(text) = item.as_str() else {
                    return Err(usage(format!(
                        "Option \"{key}\" is of type \"{kind}\", but the item at index \
                         \"{index}\" of the list is not a string."
                    )));
                };
                if *index != expected.to_string() {
                    return Err(usage(format!(
                        "Option \"{key}\" is of type \"{kind}\", but the value is not a list: \
                         it is a map with unnatural or sparse keys."
                    )));
                }
                if kind == "list<regex>"
                    && phab_remarkup::preg::compile_php(text.as_bytes()).is_none()
                {
                    return Err(usage(format!(
                        "Option \"{key}\" is of type \"{kind}\" and must be set to a list of \
                         valid regular expressions, but \"{text}\" is not a valid regular \
                         expression."
                    )));
                }
            }
            Ok(())
        }
        _ => Err(Error::Unsupported(format!("the checks of {kind}"))),
    }
}

/// `PhabricatorApplicationConfigOptions::validateOption()`: nothing to check
/// for the default or null, else the group's `didValidateOption()`.
pub fn validate_option(option: &ConfigOption, value: &Value) -> Result<(), Error> {
    // `===`: the same types, keys and order, which serde's equality is
    // with `preserve_order`, but for PHP's lists being arrays.
    if phab_core::php::round_trip(value) == phab_core::php::round_trip(&option.default)
        || value.is_null()
    {
        return Ok(());
    }
    let key = option.key.as_str();
    match key {
        "phabricator.base-uri" | "phabricator.production-uri" => {
            check_site_uri(key, value.as_str().unwrap_or_default(), true)
        }
        "security.alternate-file-domain" => {
            check_site_uri(key, value.as_str().unwrap_or_default(), false)
        }
        "phabricator.timezone" => {
            let zone = value.as_str().unwrap_or_default();
            if jiff::tz::TimeZone::get(zone).is_ok() {
                Ok(())
            } else {
                Err(usage(format!(
                    "Config option \"{key}\" is invalid. The timezone identifier must be a valid \
                     timezone identifier recognized by PHP, like \"America/Los_Angeles\"."
                )))
            }
        }
        _ if option.group_key.as_deref() == Some("reviewhelper") => Err(Error::Unsupported(
            format!("the checks of the Review Helper options ({key})"),
        )),
        _ => Ok(()),
    }
}

/// The protocol, domain and path of a URI, as `PhutilURI` parses them.
fn split_uri(uri: &str) -> (&str, &str, &str) {
    let Some((protocol, rest)) = uri.split_once("://") else {
        return ("", "", uri);
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let domain = host.split(':').next().unwrap_or_default();
    let path = rest[end..].split(['?', '#']).next().unwrap_or_default();
    (protocol, domain, path)
}

/// The checks of `phabricator.base-uri` (`PhabricatorCoreConfigOptions`)
/// and `security.alternate-file-domain` (`PhabricatorSecurityConfigOptions`),
/// with their slightly different messages.
fn check_site_uri(key: &str, uri: &str, core: bool) -> Result<(), Error> {
    let (protocol, domain, path) = split_uri(uri);
    if protocol != "http" && protocol != "https" {
        return Err(usage(if core {
            format!(
                "Config option \"{key}\" is invalid. The URI must start with \"http://\" or \
                 \"https://\"."
            )
        } else {
            format!(
                "Config option '{key}' is invalid. The URI must start with 'http://' or \
                 'https://'."
            )
        }));
    }
    if !domain.contains('.') {
        return Err(usage(if core {
            format!(
                "Config option \"{key}\" is invalid. The URI must contain a dot (\".\"), like \
                 \"http://example.com/\", not just a bare name like \"http://example/\". Some \
                 web browsers will not set cookies on domains with no TLD."
            )
        } else {
            format!(
                "Config option '{key}' is invalid. The URI must contain a dot ('.'), like \
                 'http://example.com/', not just a bare name like 'http://example/'. Some web \
                 browsers will not set cookies on domains with no TLD."
            )
        }));
    }
    if !path.is_empty() && path != "/" {
        // The core message has stray quotes from PHP's own source.
        return Err(usage(if core {
            format!(
                "Config option '{key}' is invalid. The URI must NOT have a path, e.g. \
                 'http://devtools.example.com/' is OK, but 'http://example.com/devtools/' is \
                 not. This software must be '.\n            'installed on an entire domain; it \
                 can not be installed on a path."
            )
        } else {
            format!(
                "Config option '{key}' is invalid. The URI must NOT have a path, e.g. \
                 'http://devtools.example.com/' is OK, but 'http://example.com/devtools/' is \
                 not. This software must be installed on an entire domain; it can not be \
                 installed on a path."
            )
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::catalog::option;

    fn parse(key: &str, raw: &str) -> Result<Value, Error> {
        from_command_line(option(key).unwrap(), raw)
    }

    #[rstest]
    #[case("phabricator.show-prototypes", "true", json!(true))]
    #[case("storage.mysql-engine.max-size", "-12", json!(-12))]
    #[case("phabricator.base-uri", "http://x.y/", json!("http://x.y/"))]
    #[case("email.default", "2", json!("2"))]
    #[case("phabricator.allowed-uris", "[\"a\"]", json!(["a"]))]
    #[case("config.lock", "[\"a\", \"b\"]", json!({"a": true, "b": true}))]
    fn command_line_values(#[case] key: &str, #[case] raw: &str, #[case] expected: Value) {
        assert_eq!(parse(key, raw).unwrap(), expected);
    }

    #[rstest]
    #[case("phabricator.show-prototypes", "yes")]
    #[case("storage.mysql-engine.max-size", "1.5")]
    #[case("email.default", "7")]
    #[case("phabricator.allowed-uris", "\"a\"")]
    #[case("phabricator.allowed-uris", "[1]")]
    #[case("phabricator.allowed-uris", "{\"x\": \"a\"}")]
    #[case("config.lock", "{\"x\": \"a\"}")]
    fn command_line_errors(#[case] key: &str, #[case] raw: &str) {
        assert!(
            matches!(parse(key, raw), Err(Error::Usage(_))),
            "{key} {raw}"
        );
    }

    #[test]
    fn custom_types_are_left_to_php() {
        assert!(matches!(
            parse("policy.locked", "{}"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(parse("policy.locked", "1"), Err(Error::Usage(_))));
    }

    #[rstest]
    #[case("phabricator.base-uri", "http://example.com/", true)]
    #[case("phabricator.base-uri", "ftp://example.com/", false)]
    #[case("phabricator.base-uri", "http://example/", false)]
    #[case("phabricator.base-uri", "http://example.com/x/", false)]
    #[case(
        "security.alternate-file-domain",
        "https://u@files.example.com:8/",
        true
    )]
    #[case("phabricator.timezone", "Europe/Paris", true)]
    #[case("phabricator.timezone", "Nowhere/Land", false)]
    fn group_checks(#[case] key: &str, #[case] value: &str, #[case] ok: bool) {
        let result = validate_option(option(key).unwrap(), &json!(value));
        assert_eq!(result.is_ok(), ok, "{key} {value}");
    }
}
