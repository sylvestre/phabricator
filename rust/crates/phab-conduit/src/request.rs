//! `PhabricatorConduitAPIController::decodeConduitParams()`.

use serde_json::{Map, Value};

use crate::{
    HttpRequest,
    form::{self, FormData, FormValue},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    Params {
        metadata: Map<String, Value>,
        params: Map<String, Value>,
        strictly_typed: bool,
    },
    /// An exception PHP turns into `ERR-CONDUIT-CORE`.
    CoreError(String),
    Unsupported(&'static str),
}

/// `ConduitAPIMethod::getParameterMetadataKey()`.
pub fn metadata_key(key: &str) -> Option<&str> {
    if let Some(rest) = key.strip_prefix("api.") {
        return Some(rest);
    }
    match key {
        "access_token" | "scope" | "output" | "__conduit__" | "code" | "params" => Some(key),
        _ => None,
    }
}

/// The request data (`$_POST + $_GET`), or `None` for multipart bodies,
/// which are left to PHP.
pub fn request_data(request: &HttpRequest) -> Option<FormData> {
    let is_multipart = request.content_type.as_deref().is_some_and(|content_type| {
        content_type
            .get(..19)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("multipart/form-data"))
    });
    if is_multipart {
        return None;
    }
    Some(form::merge(
        form::parse_query_string(&request.body),
        form::parse_query_string(&request.query),
    ))
}

/// `AphrontRequest::getStr()`.
pub fn get_str(data: &FormData, key: &str) -> Option<String> {
    form::get(data, key)
        .and_then(FormValue::as_str)
        .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
}

fn into_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        // A PHP list is an array with integer keys.
        Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(index, item)| (index.to_string(), item))
            .collect(),
        _ => Map::new(),
    }
}

fn split_metadata(mut params: Map<String, Value>) -> (Map<String, Value>, Map<String, Value>) {
    let metadata = params
        .shift_remove("__conduit__")
        .map(into_map)
        .unwrap_or_default();
    (metadata, params)
}

pub fn decode(request: &HttpRequest, data: &FormData, method: &str) -> Decoded {
    if request.content_type.as_deref() == Some("application/json") {
        return Decoded::CoreError(
            "Use form-encoded data to submit parameters to Conduit endpoints. Sending a \
             JSON-encoded body and setting 'Content-Type': 'application/json' is not currently \
             supported."
                .to_owned(),
        );
    }

    // Parameters from the API console: `params[name]=<json>`.
    if let Some(FormValue::Array(entries)) = form::get(data, "params") {
        let mut params = Map::new();
        for (key, value) in entries {
            let Some(text) = value.as_str() else {
                return Decoded::Unsupported("nested console parameters");
            };
            let text = if text.is_empty() { "null" } else { text };
            match serde_json::from_str::<Value>(text) {
                Ok(decoded) => {
                    params.insert(key.clone(), decoded);
                }
                // json_decode() fails on "NULL", which PHP still accepts.
                Err(_) if text.eq_ignore_ascii_case("null") => {
                    params.insert(key.clone(), Value::Null);
                }
                Err(_) => {
                    return Decoded::CoreError(format!(
                        "The value for parameter '{key}' is not valid JSON. All parameters must \
                         be encoded as JSON values, including strings (which means you need to \
                         surround them in double quotes). Check your syntax. Value was: {text}."
                    ));
                }
            }
        }
        let (metadata, params) = split_metadata(params);
        return Decoded::Params {
            metadata,
            params,
            strictly_typed: true,
        };
    }

    // A single `params` field holding the JSON-encoded dictionary.
    if let Some(json) = get_str(data, "params").filter(|json| !json.is_empty()) {
        return match serde_json::from_str::<Value>(&json) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => {
                let (metadata, params) = split_metadata(into_map(value));
                Decoded::Params {
                    metadata,
                    params,
                    strictly_typed: true,
                }
            }
            _ => Decoded::CoreError(format!(
                "Invalid parameter information was passed to method '{method}'."
            )),
        };
    }

    // Plain HTTP key-value pairs.
    let mut metadata = Map::new();
    let mut params = Map::new();
    for (key, value) in data {
        if key.starts_with("__") {
            continue;
        }
        match metadata_key(key) {
            Some(meta) => {
                metadata.insert(meta.to_owned(), value.to_json());
            }
            None => {
                params.insert(key.clone(), value.to_json());
            }
        }
    }
    Decoded::Params {
        metadata,
        params,
        strictly_typed: false,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn decode_body(body: &str) -> Decoded {
        let request = HttpRequest {
            body: body.to_owned(),
            query: "__path__=/api/x".to_owned(),
            ..HttpRequest::default()
        };
        let data = request_data(&request).unwrap();
        decode(&request, &data, "x")
    }

    fn params(metadata: Value, params: Value, strictly_typed: bool) -> Decoded {
        Decoded::Params {
            metadata: into_map(metadata),
            params: into_map(params),
            strictly_typed,
        }
    }

    #[rstest]
    #[case(
        "params=%7B%22limit%22%3A3%2C%22__conduit__%22%3A%7B%22token%22%3A%22t%22%7D%7D",
        params(json!({"token": "t"}), json!({"limit": 3}), true)
    )]
    #[case(
        "params[limit]=3&params[view]=%22data%22&params[after]=",
        params(json!({}), json!({"limit": 3, "view": "data", "after": null}), true)
    )]
    #[case(
        "api.token=t&limit=3&ids[]=1&output=json&__ajax__=1",
        params(json!({"token": "t", "output": "json"}), json!({"limit": "3", "ids": ["1"]}), false)
    )]
    #[case("params=", params(json!({"params": ""}), json!({}), false))]
    #[case(
        "params=5",
        Decoded::CoreError("Invalid parameter information was passed to method 'x'.".to_owned())
    )]
    #[case(
        "params=%7Bnope",
        Decoded::CoreError("Invalid parameter information was passed to method 'x'.".to_owned())
    )]
    fn decodes_like_php(#[case] body: &str, #[case] expected: Decoded) {
        assert_eq!(decode_body(body), expected);
    }

    #[test]
    fn rejects_unquoted_console_strings() {
        assert_eq!(
            decode_body("params[view]=data"),
            Decoded::CoreError(
                "The value for parameter 'view' is not valid JSON. All parameters must be \
                 encoded as JSON values, including strings (which means you need to surround \
                 them in double quotes). Check your syntax. Value was: data."
                    .to_owned()
            )
        );
    }

    #[test]
    fn rejects_json_bodies() {
        let request = HttpRequest {
            content_type: Some("application/json".to_owned()),
            ..HttpRequest::default()
        };
        assert!(matches!(
            decode(&request, &FormData::new(), "x"),
            Decoded::CoreError(_)
        ));
    }

    #[test]
    fn leaves_multipart_to_php() {
        let request = HttpRequest {
            content_type: Some("multipart/form-data; boundary=x".to_owned()),
            ..HttpRequest::default()
        };
        assert!(request_data(&request).is_none());
    }
}
