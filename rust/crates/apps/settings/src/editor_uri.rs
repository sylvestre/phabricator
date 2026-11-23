//! `PhabricatorEditorURIEngine::validatePattern()`: what the External
//! Editor panel accepts as the `editor` setting, with PHP's messages.

use phab_core::{Config, php};
use serde_json::Value;

/// `uri.allowed-editor-protocols`.
pub const PROTOCOLS_KEY: &str = "uri.allowed-editor-protocols";

/// The default of `uri.allowed-editor-protocols`.
const DEFAULT_PROTOCOLS: [&str; 11] = [
    "http", "https", "txmt", "mvim", "vim", "subl", "emacs", "editor", "vscode", "idea", "zed",
];

/// `getVariableDefinitions()`: each variable, what replaces it and an
/// example.
pub const VARIABLES: [(&str, &str, &str); 7] = [
    ("f", "File Name", "path/to/source.c"),
    ("l", "Line Number", "777"),
    ("n", "Repository Short Name", "arcanist"),
    ("d", "Repository ID", "42"),
    ("p", "Repository PHID", "PHID-REPO-abcdefghijklmnopqrst"),
    ("r", "Repository Callsign", "XYZ"),
    ("%", "Literal Percent Symbol", "%"),
];

/// The protocols the configuration allows, sorted (`sort()` of the keys).
pub fn allowed_protocols(config: &Config) -> Vec<String> {
    let mut protocols: Vec<String> = match config.value(PROTOCOLS_KEY) {
        None => DEFAULT_PROTOCOLS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        // A list's keys are its indexes.
        Some(Value::Array(items)) => (0..items.len()).map(|index| index.to_string()).collect(),
        Some(_) => Vec::new(),
    };
    protocols.sort();
    protocols
}

/// `!empty($allowed_protocols[$protocol])`.
fn is_allowed(config: &Config, protocol: &str) -> bool {
    match config.value(PROTOCOLS_KEY) {
        None => DEFAULT_PROTOCOLS.contains(&protocol),
        Some(Value::Object(map)) => map.get(protocol).is_some_and(php::is_truthy),
        Some(_) => false,
    }
}

/// A token of `newPatternTokens()`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(Vec<u8>),
    Variable(u8),
}

/// `validatePattern()` of a non-empty pattern: `Err` with the message of
/// PHP's `PhabricatorEditorURIParserException`, or `None` in the message's
/// place when PHP fails another way (a URI `PhutilURI` can't parse),
/// which the callers leave to PHP.
pub fn validate(config: &Config, pattern: &str) -> Result<(), Option<String>> {
    let bytes = pattern.as_bytes();
    // `newPatternTokens()`.
    let mut tokens = Vec::new();
    let mut literal = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(variable) = bytes.get(index + 1) else {
                return Err(Some(format!(
                    "Editor pattern \"{pattern}\" is invalid: the final character in a \
                     pattern may not be an unencoded percent symbol (\"%\"). Use \"%%\" to \
                     encode a literal percent symbol."
                )));
            };
            if !literal.is_empty() {
                tokens.push(Token::Literal(std::mem::take(&mut literal)));
            }
            tokens.push(Token::Variable(*variable));
            index += 2;
        } else {
            literal.push(bytes[index]);
            index += 1;
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    for token in &tokens {
        if let Token::Variable(variable) = token
            && !VARIABLES
                .iter()
                .any(|(name, _, _)| name.as_bytes() == [*variable])
        {
            let variable = String::from_utf8_lossy(&[*variable]).into_owned();
            return Err(Some(format!(
                "Editor pattern \"{pattern}\" is invalid: the pattern contains an \
                 unrecognized variable (\"%{variable}\"). Use \"%%\" to encode a literal \
                 percent symbol."
            )));
        }
    }
    // `newTokensWithVariables()` with `%%`, and the first literal.
    let mut first_literal: Option<Vec<u8>> = None;
    for token in &tokens {
        match token {
            Token::Literal(text) => first_literal.get_or_insert_with(Vec::new).extend(text),
            Token::Variable(b'%') => first_literal.get_or_insert_with(Vec::new).push(b'%'),
            Token::Variable(_) => break,
        }
    }
    let Some(first_literal) = first_literal else {
        return Err(Some(format!(
            "Editor pattern \"{pattern}\" is invalid: the pattern must begin with a valid \
             editor protocol, but begins with a variable. This is very sneaky and also very \
             forbidden."
        )));
    };
    let protocol = phab_remarkup::uri::protocol(&first_literal).map_err(|_| None)?;
    if protocol.is_empty() {
        return Err(Some(format!(
            "Editor pattern \"{pattern}\" is invalid: the pattern must begin with a valid \
             editor protocol, but does not begin with a recognized protocol string."
        )));
    }
    let protocol = String::from_utf8_lossy(&protocol).into_owned();
    if !is_allowed(config, &protocol) {
        return Err(Some(format!(
            "Editor pattern \"{pattern}\" is invalid: the pattern must begin with a valid \
             editor protocol, but the protocol \"{protocol}://\" is not allowed."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::{Map, json};

    use super::*;

    #[rstest]
    #[case(
        "txmt://open/?url=file:///Users/alincoln/editor_links/%n/%f&line=%l",
        true
    )]
    #[case("vscode://file/%f:%l", true)]
    #[case("subl://open?url=%%%f", true)]
    #[case("subl://open?url=%25%f", false)]
    #[case("%f", false)]
    #[case("txmt://open/%", false)]
    #[case("txmt://open/%x", false)]
    #[case("javascript:alert(1)", false)]
    #[case("ftp://example.com/%f", false)]
    #[case("/local/path/%f", false)]
    fn patterns_like_php(#[case] pattern: &str, #[case] valid: bool) {
        assert_eq!(
            validate(&Config::default(), pattern).is_ok(),
            valid,
            "{pattern}"
        );
    }

    #[test]
    fn messages_like_php() {
        let config = Config::default();
        assert_eq!(
            validate(&config, "ftp://x/%f").unwrap_err().unwrap(),
            "Editor pattern \"ftp://x/%f\" is invalid: the pattern must begin with a valid \
             editor protocol, but the protocol \"ftp://\" is not allowed."
        );
        assert_eq!(
            validate(&config, "%%x").unwrap_err().unwrap(),
            "Editor pattern \"%%x\" is invalid: the pattern must begin with a valid editor \
             protocol, but does not begin with a recognized protocol string."
        );
    }

    #[test]
    fn protocols_come_from_the_configuration() {
        let mut config = Config::default();
        assert_eq!(allowed_protocols(&config).len(), 11);
        let mut values = Map::new();
        values.insert(
            PROTOCOLS_KEY.to_owned(),
            json!({"zzz": true, "ftp": false, "abc": true}),
        );
        config.push_layer("test", values);
        assert_eq!(allowed_protocols(&config), ["abc", "ftp", "zzz"]);
        assert!(validate(&config, "abc://open/%f").is_ok());
        assert!(validate(&config, "ftp://open/%f").is_err());
        assert!(validate(&config, "txmt://open/%f").is_err());
    }
}
