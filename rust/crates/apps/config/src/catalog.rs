//! The configuration options of `PhabricatorApplicationConfigOptions::
//! loadAllOptions()`, from `options.json`. Regenerate it with
//! `rust/scripts/generate-catalogs.php config` when options change.

use std::sync::OnceLock;

use phab_core::Config;
use serde::Deserialize;
use serde_json::Value;

/// An example value of an option, with what it means.
#[derive(Debug, Clone, Deserialize)]
pub struct Example {
    /// The value, as PHP shows it (a string, or a list for lists).
    pub value: Value,
    /// What it does.
    pub description: Option<String>,
}

/// A `PhabricatorConfigOption`.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigOption {
    /// The key, like `phabricator.base-uri`.
    pub key: String,
    /// The type, like `bool`, `list<string>` or `custom:<class>`.
    #[serde(rename = "type")]
    pub option_type: String,
    /// The default value.
    pub default: Value,
    /// `getSummary()`: the summary, or the description without one.
    pub summary: Option<String>,
    /// The description, in remarkup.
    pub description: Option<String>,
    /// Locked by its definition (`setLocked()`).
    pub locked: bool,
    /// Hidden by its definition (`setHidden()`).
    pub hidden: bool,
    /// `setLockedMessage()`, as HTML.
    pub locked_message: Option<String>,
    /// The name of the option's group.
    pub group: Option<String>,
    /// The key of the option's group.
    pub group_key: Option<String>,
    /// The labels of `true` and `false` for booleans.
    pub bool_options: Option<Vec<String>>,
    /// The values (and their labels) of enums.
    pub enum_options: Option<serde_json::Map<String, Value>>,
    /// The concrete subclasses of the base class, for classes.
    pub classes: Option<Vec<String>>,
    /// The base class, for classes.
    pub base_class: Option<String>,
    /// The examples.
    pub examples: Vec<Example>,
}

impl ConfigOption {
    /// `getHidden()`: hidden by definition or by `config.hide`.
    pub fn is_hidden(&self, config: &Config) -> bool {
        self.hidden || listed_in(config, "config.hide", &self.key)
    }

    /// `getLocked()`: locked by definition, hidden, or by `config.lock`.
    pub fn is_locked(&self, config: &Config) -> bool {
        self.locked || self.is_hidden(config) || listed_in(config, "config.lock", &self.key)
    }

    /// `getLockedMessage()`, as HTML.
    pub fn locked_message_html(&self) -> String {
        self.locked_message.clone().unwrap_or_else(|| {
            "This configuration is locked and can not be edited from the web interface. \
             Use <tt>./bin/config</tt> in <tt>phabricator/</tt> to edit it."
                .to_owned()
        })
    }
}

/// `idx(PhabricatorEnv::getEnvConfig($list), $key, false)`, as a boolean.
fn listed_in(config: &Config, list: &str, key: &str) -> bool {
    config
        .object(list)
        .and_then(|map| map.get(key))
        .is_some_and(phab_core::php::is_truthy)
}

/// Every option, sorted by key.
pub fn options() -> &'static [ConfigOption] {
    static OPTIONS: OnceLock<Vec<ConfigOption>> = OnceLock::new();
    OPTIONS.get_or_init(|| {
        let mut options: Vec<ConfigOption> = serde_json::from_str(include_str!("../options.json"))
            .expect("options.json is a list of options");
        options.sort_by(|left, right| left.key.cmp(&right.key));
        options
    })
}

/// The option `key`, if PHP defines it.
pub fn option(key: &str) -> Option<&'static ConfigOption> {
    let options = options();
    options
        .binary_search_by(|option| option.key.as_str().cmp(key))
        .ok()
        .map(|index| &options[index])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_catalog_loads_and_finds_options() {
        assert!(options().len() > 100);
        let option = option("phabricator.base-uri").unwrap();
        assert_eq!(option.option_type, "string");
        assert!(option.locked);
        assert!(super::option("no.such-key").is_none());
    }

    #[test]
    fn config_lock_and_hide_apply() {
        let mut config = Config::default();
        let option = option("phabricator.show-prototypes").unwrap();
        assert!(!option.is_hidden(&config));
        config.push_layer(
            "test",
            json!({"config.hide": {"phabricator.show-prototypes": true}})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(option.is_hidden(&config));
        assert!(option.is_locked(&config));
    }
}
