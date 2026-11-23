//! `cluster.mailers` and `PhabricatorMetaMTAMail::newMailers()`.

use std::collections::BTreeMap;

use phab_core::Config;
use rand::seq::SliceRandom;
use serde_json::{Map, Value};

use crate::{Error, php};

/// `PhabricatorMailEmailMessage::MESSAGETYPE`, the only medium of every
/// adapter.
pub const MESSAGE_TYPE_EMAIL: &str = "email";

/// The adapter of a mailer (`PhabricatorMailAdapter::ADAPTERTYPE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adapter {
    /// `PhabricatorMailTestAdapter` (`test`): records the message.
    Test,
    /// `PhabricatorMailSMTPAdapter` (`smtp`).
    Smtp(SmtpOptions),
    /// An adapter Rust knows the options of but can't send with
    /// (`ses`): mail it would deliver is left to PHP.
    Unported(&'static str),
}

/// The options of an SMTP mailer, defaults applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpOptions {
    /// `host`.
    pub host: Option<String>,
    /// `port` (25).
    pub port: i64,
    /// `user`.
    pub user: Option<String>,
    /// `password`.
    pub password: Option<String>,
    /// `protocol` (`tls` or `ssl`).
    pub protocol: Option<String>,
    /// `message-id`.
    pub message_id: Option<bool>,
}

/// One configured mailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailer {
    /// `key`.
    pub key: String,
    /// The adapter and its options.
    pub adapter: Adapter,
    /// The priority, made up when not configured.
    pub priority: i64,
    supports_outbound: bool,
    media: Option<Vec<String>>,
}

impl Mailer {
    /// `supportsMessageIDHeader()`.
    pub fn supports_message_id(&self) -> bool {
        match &self.adapter {
            // The test adapter returns its unset property, `null`.
            Adapter::Test | Adapter::Unported(_) => false,
            Adapter::Smtp(options) => {
                guess_if_host_supports_message_id(options.message_id, options.host.as_deref())
            }
        }
    }

    /// `supportsMessageType()`: every adapter sends only email.
    fn supports_message_type(&self, message_type: &str) -> bool {
        message_type == MESSAGE_TYPE_EMAIL
            && self
                .media
                .as_ref()
                .is_none_or(|media| media.is_empty() || media.iter().any(|m| m == message_type))
    }
}

/// `guessIfHostSupportsMessageID()`.
fn guess_if_host_supports_message_id(config: Option<bool>, host: Option<&str>) -> bool {
    if let Some(config) = config {
        return config;
    }
    if let Some(host) = host {
        let host = host.to_lowercase();
        if [".amazonaws.com", ".postmarkapp.com", ".sendgrid.net"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        {
            return false;
        }
    }
    true
}

fn unsupported(reason: &str) -> Error {
    Error::unsupported(format!("cluster.mailers: {reason}"))
}

/// The options of a spec merged over the adapter's defaults
/// (`idx($spec, 'options', array()) + $defaults`), checked as
/// `validateOptions()` checks them. A value it would reject is declined.
fn adapter(kind: &str, options: &Map<String, Value>) -> Result<Adapter, Error> {
    let nullable_string = |key: &str| match options.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(unsupported("an option of the wrong type")),
    };
    let only = |keys: &[&str]| {
        if options.keys().all(|key| keys.contains(&key.as_str())) {
            Ok(())
        } else {
            Err(unsupported("an unknown mailer option"))
        }
    };
    match kind {
        "test" => {
            only(&[])?;
            Ok(Adapter::Test)
        }
        "smtp" => {
            only(&["host", "port", "user", "password", "protocol", "message-id"])?;
            let port = match options.get("port") {
                None => 25,
                Some(Value::Number(number)) if number.is_i64() => number.as_i64().unwrap_or(25),
                Some(_) => return Err(unsupported("an SMTP port which is not an integer")),
            };
            let message_id = match options.get("message-id") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(flag)) => Some(*flag),
                Some(_) => return Err(unsupported("an option of the wrong type")),
            };
            Ok(Adapter::Smtp(SmtpOptions {
                host: nullable_string("host")?,
                port,
                user: nullable_string("user")?,
                password: nullable_string("password")?,
                protocol: nullable_string("protocol")?,
                message_id,
            }))
        }
        "ses" => {
            let keys = ["access-key", "secret-key", "region", "endpoint"];
            only(&keys)?;
            // Each is `string`, not nullable: a missing one throws.
            if keys
                .iter()
                .all(|key| matches!(options.get(*key), Some(Value::String(_))))
            {
                Ok(Adapter::Unported("ses"))
            } else {
                Err(unsupported("SES options PHP rejects"))
            }
        }
        "sendmail" | "mailgun" | "postmark" | "sendgrid" => {
            Err(Error::unsupported(format!("the {kind} mailer")))
        }
        // `Unknown mailer ("%s")!`, thrown before anything is written.
        _ => Err(unsupported("an unknown mailer type")),
    }
}

/// `newMailers()` with the `outbound` and `media` constraints when
/// `message_type` is given, none otherwise. Within a priority the order is
/// random, as PHP's `shuffle()`.
pub fn new_mailers(config: &Config, message_type: Option<&str>) -> Result<Vec<Mailer>, Error> {
    let specs = match config.value("cluster.mailers") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => php::values(value).ok_or_else(|| unsupported("not a list"))?,
    };
    let mut mailers = Vec::new();
    let mut next_priority = -1;
    for spec in specs {
        let Value::Object(spec) = spec else {
            return Err(unsupported("a mailer which is not a dictionary"));
        };
        let Some(Value::String(kind)) = spec.get("type") else {
            return Err(unsupported("a mailer without a type"));
        };
        let Some(Value::String(key)) = spec.get("key") else {
            return Err(unsupported("a mailer without a key"));
        };
        let priority = match spec.get("priority") {
            None | Some(Value::Null) => 0,
            Some(Value::Number(number)) if number.is_i64() => number.as_i64().unwrap_or(0),
            Some(_) => return Err(unsupported("a priority which is not an integer")),
        };
        let priority = if priority == 0 {
            next_priority -= 1;
            next_priority + 1
        } else {
            priority
        };
        let options = match spec.get("options") {
            None => Map::new(),
            Some(Value::Object(options)) => options.clone(),
            Some(Value::Array(list)) if list.is_empty() => Map::new(),
            Some(_) => return Err(unsupported("options which are not a dictionary")),
        };
        let flag = |name: &str| match spec.get(name) {
            None => Ok(true),
            Some(Value::Bool(flag)) => Ok(*flag),
            Some(_) => Err(unsupported("a flag which is not a boolean")),
        };
        // `inbound` is only read by inbound mail.
        flag("inbound")?;
        let supports_outbound = flag("outbound")?;
        let media = match spec.get("media") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let media = php::strings(value).ok_or_else(|| unsupported("media"))?;
                // `setMedia()` throws for a medium the adapter can't send.
                if media.iter().any(|medium| medium != MESSAGE_TYPE_EMAIL) {
                    return Err(unsupported("a medium other than email"));
                }
                Some(media)
            }
        };
        mailers.push(Mailer {
            key: key.clone(),
            adapter: adapter(kind, &options)?,
            priority,
            supports_outbound,
            media,
        });
    }
    if let Some(message_type) = message_type {
        mailers.retain(|mailer| mailer.supports_outbound);
        mailers.retain(|mailer| mailer.supports_message_type(message_type));
    }
    // `mgroup()` then `krsort()`: highest priority first.
    let mut groups: BTreeMap<i64, Vec<Mailer>> = BTreeMap::new();
    for mailer in mailers {
        groups.entry(mailer.priority).or_default().push(mailer);
    }
    let mut rng = rand::rng();
    let mut sorted = Vec::new();
    for (_, mut group) in groups.into_iter().rev() {
        group.shuffle(&mut rng);
        sorted.extend(group);
    }
    Ok(sorted)
}

/// `array_select_keys()` of the mailers by key, for `mailers.try`.
pub fn select(mailers: Vec<Mailer>, keys: &[String]) -> Vec<Mailer> {
    let mut by_key: Vec<Option<Mailer>> = mailers.into_iter().map(Some).collect();
    let mut selected = Vec::new();
    for key in keys {
        // `mpull($mailers, null, 'getKey')`: the last mailer of a key wins.
        if let Some(slot) = by_key
            .iter_mut()
            .rev()
            .find(|slot| slot.as_ref().is_some_and(|mailer| &mailer.key == key))
            && let Some(mailer) = slot.take()
        {
            // Later mailers with the same key are gone from the map too.
            by_key
                .iter_mut()
                .filter(|slot| slot.as_ref().is_some_and(|other| &other.key == key))
                .for_each(|slot| *slot = None);
            selected.push(mailer);
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(mailers: Value) -> Config {
        let mut config = Config::default();
        let mut layer = Map::new();
        layer.insert("cluster.mailers".to_owned(), mailers);
        config.push_layer("test", layer);
        config
    }

    #[test]
    fn sorts_by_priority_and_filters_outbound() {
        let config = config(json!([
            {"key": "a", "type": "test"},
            {"key": "b", "type": "smtp", "priority": 10, "options": {"host": "x.example"}},
            {"key": "c", "type": "test", "outbound": false},
        ]));
        let keys = |mailers: Vec<Mailer>| -> Vec<String> {
            mailers.into_iter().map(|mailer| mailer.key).collect()
        };
        assert_eq!(
            keys(new_mailers(&config, Some("email")).unwrap()),
            ["b", "a"]
        );
        assert_eq!(keys(new_mailers(&config, None).unwrap()), ["b", "a", "c"]);
        assert_eq!(
            keys(new_mailers(&config, Some("sms")).unwrap()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn declines_what_php_rejects_or_cannot_be_sent() {
        assert!(new_mailers(&config(json!([{"key": "a", "type": "nope"}])), None).is_err());
        assert!(
            new_mailers(
                &config(json!([{"key": "a", "type": "test", "options": {"x": 1}}])),
                None
            )
            .is_err()
        );
        assert!(new_mailers(&config(json!([{"key": "a", "type": "ses"}])), None).is_err());
        let ses = json!([{"key": "a", "type": "ses", "options": {
            "access-key": "k", "secret-key": "s", "region": "r", "endpoint": "e"}}]);
        assert_eq!(
            new_mailers(&config(ses), None).unwrap()[0].adapter,
            Adapter::Unported("ses")
        );
    }

    #[test]
    fn guesses_message_id_support() {
        assert!(guess_if_host_supports_message_id(
            None,
            Some("smtp.example.com")
        ));
        assert!(!guess_if_host_supports_message_id(
            None,
            Some("email-smtp.us-west-2.AMAZONAWS.com")
        ));
        assert!(guess_if_host_supports_message_id(
            Some(true),
            Some("x.sendgrid.net")
        ));
    }

    #[test]
    fn selects_mailers_in_the_order_tried() {
        let config = config(json!([
            {"key": "a", "type": "test", "priority": 2},
            {"key": "b", "type": "test", "priority": 1},
        ]));
        let mailers = new_mailers(&config, Some("email")).unwrap();
        let selected = select(mailers, &["b".to_owned(), "z".to_owned(), "a".to_owned()]);
        let keys: Vec<&str> = selected.iter().map(|mailer| mailer.key.as_str()).collect();
        assert_eq!(keys, ["b", "a"]);
    }
}
