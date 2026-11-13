//! Layered configuration compatible with `PhabricatorEnv`.
//!
//! PHP builds a stack of sources (defaults, `conf/local/local.json`, then the
//! `phabricator_config.config_entry` table) and reads each key from the
//! topmost source that defines it. `Config` keeps the same stack. The
//! database layer is pushed by `phab-db` once a connection is available.
//!
//! PHP environment files (`conf/<env>.conf.php`) are not read: they are PHP
//! code, and the only one in use (`moz-extensions.conf.php`) configures PHP
//! library loading and event listeners.

use std::path::Path;
use std::{fs, io};

use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::php;

/// Errors from loading or reading the configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("configuration file {path} exists but could not be read")]
    Read {
        /// Path of the file.
        path: String,
        /// The I/O error.
        source: io::Error,
    },
    /// The file is not valid JSON.
    #[error("configuration file {path} is not valid JSON")]
    Parse {
        /// Path of the file.
        path: String,
        /// The JSON error.
        source: serde_json::Error,
    },
    /// The file holds JSON other than an object.
    #[error("configuration file {path} must contain a JSON object")]
    NotAnObject {
        /// Path of the file.
        path: String,
    },
    /// `phabricator.base-uri` is not set.
    #[error("define 'phabricator.base-uri' in your configuration")]
    MissingBaseUri,
    /// `security.hmac-key` is not set.
    #[error("set a 'security.hmac-key' in your configuration")]
    MissingHmacKey,
}

#[derive(Debug, Clone)]
struct Layer {
    name: String,
    values: Map<String, Value>,
}

/// The stack of configuration sources, as `PhabricatorEnv` keeps it.
#[derive(Debug, Clone)]
pub struct Config {
    layers: Vec<Layer>,
}

/// Defaults of the options the Rust services read, copied from the PHP option
/// definitions (`src/applications/*/config/*ConfigOptions.php`).
fn default_values() -> Map<String, Value> {
    let defaults = json!({
        "phabricator.base-uri": null,
        "phabricator.production-uri": null,
        "phabricator.show-prototypes": false,
        "phabricator.uninstalled-applications": {},
        "phabricator.application-settings": {},
        "phabricator.serious-business": false,
        "security.alternate-file-domain": null,
        // Single-quoted in PHP, so the backslashes are literal.
        "security.hmac-key": r"[D\t~Y7eNmnQGJ;rnH6aF;m2!vJ8@v8C=Cs:aQS\.Qw",
        "policy.allow-public": false,
        "policy.locked": {},
        "auth.require-email-verification": false,
        "auth.email-domains": [],
        "cluster.addresses": [],
        "cluster.instance": null,
        "cluster.read-only": false,
        "mysql.host": "localhost",
        "mysql.port": null,
        "mysql.user": "root",
        "mysql.pass": null,
        "storage.default-namespace": "phabricator",
        "uri.allowed-protocols": {"http": true, "https": true, "mailto": true},
        "remarkup.ignored-object-names": r"/^(Q|V|M|P)\d$/",
        "bugzilla.url": "https://bugzilla.allizom.org",
    });
    match defaults {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            layers: vec![Layer {
                name: "Global Default".to_owned(),
                values: default_values(),
            }],
        }
    }
}

impl Config {
    /// Push a source on top of the stack; `name` is reported by [`Config::source_of`].
    pub fn push_layer(&mut self, name: impl Into<String>, values: Map<String, Value>) {
        self.layers.push(Layer {
            name: name.into(),
            values,
        });
    }

    /// Push `conf/local/local.json`. A missing file is not an error, as in
    /// `PhabricatorConfigLocalSource`.
    pub fn push_local_file(&mut self, path: &Path) -> Result<(), ConfigError> {
        let display = path.display().to_string();
        let data = match fs::read_to_string(path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: display,
                    source,
                });
            }
        };
        let parsed: Value = serde_json::from_str(&data).map_err(|source| ConfigError::Parse {
            path: display.clone(),
            source,
        })?;
        match parsed {
            Value::Object(values) => {
                self.push_layer("Local Config", values);
                Ok(())
            }
            // PHP decodes `[]` to an empty array, which is a valid empty
            // source.
            Value::Array(items) if items.is_empty() => Ok(()),
            _ => Err(ConfigError::NotAnObject { path: display }),
        }
    }

    /// Value of `key` from the topmost layer which defines it.
    pub fn value(&self, key: &str) -> Option<&Value> {
        self.layers
            .iter()
            .rev()
            .find_map(|layer| layer.values.get(key))
    }

    /// Name of the layer `key` comes from, for diagnostics.
    pub fn source_of(&self, key: &str) -> Option<&str> {
        self.layers
            .iter()
            .rev()
            .find(|layer| layer.values.contains_key(key))
            .map(|layer| layer.name.as_str())
    }

    /// The value of `key` if it is a string.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.value(key).and_then(Value::as_str)
    }

    /// The value of `key` if it is a non-empty string.
    pub fn nonempty_string(&self, key: &str) -> Option<&str> {
        self.string(key).filter(|text| !text.is_empty())
    }

    /// Whether the value of `key` is truthy in PHP.
    pub fn truthy(&self, key: &str) -> bool {
        self.value(key).is_some_and(php::is_truthy)
    }

    /// The value of `key` if it is a JSON object.
    pub fn object(&self, key: &str) -> Option<&Map<String, Value>> {
        self.value(key).and_then(Value::as_object)
    }

    /// `policy.allow-public`.
    pub fn allow_public(&self) -> bool {
        self.truthy("policy.allow-public")
    }

    /// `cluster.read-only`.
    pub fn is_read_only(&self) -> bool {
        self.truthy("cluster.read-only")
    }

    /// `security.hmac-key`, which must be set.
    pub fn hmac_key(&self) -> Result<&str, ConfigError> {
        self.value("security.hmac-key")
            .filter(|value| php::is_truthy(value))
            .and_then(Value::as_str)
            .ok_or(ConfigError::MissingHmacKey)
    }

    /// `PhabricatorEnv::getAnyBaseURI()`, without the request fallback: the
    /// Rust services always run with `phabricator.base-uri` set.
    pub fn base_uri(&self) -> Result<&str, ConfigError> {
        self.value("phabricator.base-uri")
            .filter(|value| php::is_truthy(value))
            .and_then(Value::as_str)
            .ok_or(ConfigError::MissingBaseUri)
    }

    /// `PhabricatorEnv::getURI()`.
    pub fn uri(&self, path: &str) -> Result<String, ConfigError> {
        Ok(format!("{}{path}", self.base_uri()?.trim_end_matches('/')))
    }

    /// `PhabricatorEnv::getCDNURI()`: the path replaces any path of the
    /// alternate file domain (or of the base URI).
    pub fn cdn_uri(&self, path: &str) -> Result<String, ConfigError> {
        let alternate = self
            .value("security.alternate-file-domain")
            .filter(|value| php::is_truthy(value))
            .and_then(Value::as_str);
        let base = match alternate {
            Some(alternate) => alternate,
            None => self.base_uri()?,
        };
        Ok(format!("{}{path}", uri_origin(base)))
    }
}

/// The `scheme://authority` part of a URI, or the whole string if it has no
/// scheme.
fn uri_origin(uri: &str) -> &str {
    let Some(scheme_end) = uri.find("://") else {
        return uri;
    };
    let authority_start = scheme_end + 3;
    let authority_end = uri[authority_start..]
        .find(['/', '?', '#'])
        .map_or(uri.len(), |offset| authority_start + offset);
    &uri[..authority_end]
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rstest::rstest;

    use super::*;

    fn config_with(values: Value) -> Config {
        let mut config = Config::default();
        if let Value::Object(map) = values {
            config.push_layer("test", map);
        }
        config
    }

    #[test]
    fn later_layers_override_earlier_ones() {
        let mut config = config_with(json!({"mysql.host": "local"}));
        assert_eq!(config.string("mysql.host"), Some("local"));
        config.push_layer(
            "Database",
            Map::from_iter([("mysql.host".to_owned(), json!("db"))]),
        );
        assert_eq!(config.string("mysql.host"), Some("db"));
        assert_eq!(config.source_of("mysql.host"), Some("Database"));
        assert_eq!(config.source_of("mysql.user"), Some("Global Default"));
    }

    #[test]
    fn default_hmac_key_has_literal_backslashes() {
        let config = Config::default();
        let key = config.hmac_key().unwrap();
        assert_eq!(key.len(), 43);
        // Expected digest computed by PHP with the option's default value.
        assert_eq!(
            crate::hash::weak_digest(b"abc", key.as_bytes()).unwrap(),
            "a2a5ca838a594a487bc0cd185017e4d2bf6601dc"
        );
    }

    #[rstest]
    #[case(
        "http://phabricator.test/",
        "/p/alice/",
        "http://phabricator.test/p/alice/"
    )]
    #[case("https://phab.example.com", "/", "https://phab.example.com/")]
    fn builds_uris(#[case] base: &str, #[case] path: &str, #[case] expected: &str) {
        let config = config_with(json!({ "phabricator.base-uri": base }));
        assert_eq!(config.uri(path).unwrap(), expected);
    }

    #[rstest]
    #[case(json!({"phabricator.base-uri": "http://phab.test/"}), "http://phab.test/")]
    #[case(
        json!({"phabricator.base-uri": "http://phab.test/", "security.alternate-file-domain": "https://cdn.test/some/path?x=1"}),
        "https://cdn.test/"
    )]
    #[case(
        json!({"phabricator.base-uri": "http://phab.test/", "security.alternate-file-domain": ""}),
        "http://phab.test/"
    )]
    fn builds_cdn_uris(#[case] values: Value, #[case] expected: &str) {
        assert_eq!(config_with(values).cdn_uri("/").unwrap(), expected);
    }

    #[test]
    fn missing_base_uri_is_an_error() {
        assert!(matches!(
            Config::default().uri("/"),
            Err(ConfigError::MissingBaseUri)
        ));
    }

    #[test]
    fn reads_local_json() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"policy.allow-public": true}}"#).unwrap();
        let mut config = Config::default();
        config.push_local_file(file.path()).unwrap();
        assert!(config.allow_public());
        assert_eq!(
            config.source_of("policy.allow-public"),
            Some("Local Config")
        );
    }

    #[test]
    fn missing_local_json_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .push_local_file(&directory.path().join("local.json"))
            .unwrap();
        assert!(!config.allow_public());
    }

    #[test]
    fn invalid_local_json_is_an_error() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{{nope").unwrap();
        let mut config = Config::default();
        assert!(matches!(
            config.push_local_file(file.path()),
            Err(ConfigError::Parse { .. })
        ));
    }
}
