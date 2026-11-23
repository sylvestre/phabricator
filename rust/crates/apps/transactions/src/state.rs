//! `loadWorkerState()`: the editor state the publish task carries
//! (`getWorkerState()` of the editor which applied the transactions).

use phab_core::php;
use serde_json::{Map, Value};

use crate::Error;

/// The worker state of a Differential editor.
#[derive(Debug, Clone)]
pub(crate) struct State {
    raw: Map<String, Value>,
}

/// A list of strings, as `array` parameters take them; `None` for anything
/// else (PHP fails, or keys matter).
fn strings(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Value::Object(entries) if entries.is_empty() => Some(Vec::new()),
        _ => None,
    }
}

impl State {
    /// `idx($data, 'state', array())`, which must be an array.
    pub(crate) fn from_data(data: &Map<String, Value>) -> Result<Self, Error> {
        let raw = match data.get("state") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(Value::Array(items)) if items.is_empty() => Map::new(),
            Some(_) => {
                return Err(Error::unsupported(
                    "a worker state which is not a dictionary",
                ));
            }
        };
        let state = Self { raw };
        // `decodeStateFromStorage()` with encodings, and the Differential
        // state which changes the mail (`loadCustomWorkerState()`).
        if state.raw.get("custom.encoding").is_some_and(php::is_truthy) {
            return Err(Error::unsupported("encoded custom worker state"));
        }
        let custom = match state.raw.get("custom") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(Value::Array(items)) if items.is_empty() => Map::new(),
            Some(_) => {
                return Err(Error::unsupported(
                    "custom worker state which is not a dictionary",
                ));
            }
        };
        if custom.get("firstBroadcast").is_some_and(php::is_truthy) {
            return Err(Error::unsupported("the first broadcast of a revision"));
        }
        if custom
            .get("changedPriorToCommitURI")
            .is_some_and(php::is_truthy)
        {
            return Err(Error::unsupported("a revision changed prior to commit"));
        }
        if state.raw.get("sendHistory").is_some_and(php::is_truthy) {
            return Err(Error::unsupported("history mail"));
        }
        if state.raw.get("webhookMap").is_some_and(php::is_truthy) {
            return Err(Error::unsupported("webhooks triggered by Herald"));
        }
        Ok(state)
    }

    /// The raw value of a property, `null` when missing.
    pub(crate) fn value(&self, key: &str) -> &Value {
        self.raw.get(key).unwrap_or(&Value::Null)
    }

    /// Whether a property is truthy.
    pub(crate) fn flag(&self, key: &str) -> bool {
        php::is_truthy(self.value(key))
    }

    /// A property PHP passes where an `array` is required.
    pub(crate) fn list(&self, key: &str) -> Result<Vec<String>, Error> {
        strings(self.value(key))
            .ok_or_else(|| Error::unsupported(format!("worker state {key} which is not a list")))
    }

    /// A property PHP reads with `if (!is_array(...)) array()`.
    pub(crate) fn lenient_list(&self, key: &str) -> Result<Vec<String>, Error> {
        match self.value(key) {
            value @ (Value::Array(_) | Value::Object(_)) => strings(value).ok_or_else(|| {
                Error::unsupported(format!("worker state {key} which is not a list"))
            }),
            _ => Ok(Vec::new()),
        }
    }

    /// `excludeMailRecipientPHIDs`, `array()` when missing.
    pub(crate) fn exclude(&self) -> Result<Vec<String>, Error> {
        match self.raw.get("excludeMailRecipientPHIDs") {
            None => Ok(Vec::new()),
            Some(_) => self.list("excludeMailRecipientPHIDs"),
        }
    }
}
