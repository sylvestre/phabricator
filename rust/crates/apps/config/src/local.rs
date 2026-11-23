//! `PhabricatorConfigLocalSource`: `conf/local/local.json`, which
//! `bin/config` reads and rewrites whole.

use std::path::{Path, PathBuf};
use std::{fs, io};

use serde_json::{Map, Value};

use crate::{Error, json};

/// `conf/local/local.json`, loaded.
#[derive(Debug, Clone)]
pub struct LocalSource {
    path: PathBuf,
    values: Map<String, Value>,
}

impl LocalSource {
    /// `loadConfig()`: a missing file is an empty source.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let values = match fs::read_to_string(path) {
            Ok(data) => match json::decode(&data) {
                Ok(Value::Object(values)) => values,
                // `[]` decodes to an empty PHP array.
                Ok(Value::Array(list)) if list.is_empty() => Map::new(),
                Ok(Value::Array(list)) => list
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| (index.to_string(), value))
                    .collect(),
                _ => {
                    return Err(Error::Local(format!(
                        "Configuration file \"{}\" exists and is readable, but the content is \
                         not valid JSON. You may have edited this file manually and introduced \
                         a syntax error by mistake. Correct the file syntax to continue.",
                        path.display()
                    )));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
            Err(error) => {
                return Err(Error::Local(format!(
                    "Configuration file \"{}\" exists, but could not be read: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path: path.to_owned(),
            values,
        })
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The value of `key`, if the file sets it.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// `setKeys()`: `$keys + $this->dictionary`, so the key moves to the
    /// top of the file; then the file is written.
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), Error> {
        let mut values = Map::new();
        values.insert(key.to_owned(), value);
        for (other, value) in std::mem::take(&mut self.values) {
            if other != key {
                values.insert(other, value);
            }
        }
        self.values = values;
        self.save()
    }

    /// `deleteKeys()`, then the file is written.
    pub fn delete(&mut self, key: &str) -> Result<(), Error> {
        self.values.shift_remove(key);
        self.save()
    }

    /// `saveConfig()`.
    fn save(&self) -> Result<(), Error> {
        fs::write(&self.path, json::encode_formatted(&self.values)).map_err(|_| {
            Error::Local(format!(
                "Local path \"{}\" is not writable. This file must be writable so that \
                 \"bin/config\" can store configuration.",
                self.path.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn set_moves_the_key_to_the_top_and_delete_removes_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local.json");
        fs::write(&path, "{\"a\": 1, \"b\": {\"x\": [1]}}").unwrap();
        let mut local = LocalSource::load(&path).unwrap();
        local.set("b", json!(true)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"b\": true,\n  \"a\": 1\n}\n"
        );
        local.set("c", json!("http://x/")).unwrap();
        local.delete("a").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"c\": \"http://x/\",\n  \"b\": true\n}\n"
        );
        assert_eq!(
            LocalSource::load(&path).unwrap().get("b"),
            Some(&json!(true))
        );
    }

    #[test]
    fn missing_and_broken_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local.json");
        assert!(LocalSource::load(&path).unwrap().get("a").is_none());
        fs::write(&path, "{").unwrap();
        assert!(matches!(LocalSource::load(&path), Err(Error::Local(_))));
    }
}
