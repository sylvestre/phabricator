//! The `{result, error_code, error_info}` response of a call.

use serde_json::{Value, json};

use crate::json;

/// `ConduitAPIResponse`.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// The method's result; `null` on errors.
    pub result: Value,
    /// The error code; `None` on success.
    pub error_code: Option<String>,
    /// The error description; `None` on success.
    pub error_info: Option<String>,
}

impl Envelope {
    /// A successful answer.
    pub fn success(result: Value) -> Self {
        Self {
            result,
            error_code: None,
            error_info: None,
        }
    }

    /// An error answer, with a `null` result.
    pub fn error(code: impl Into<String>, info: impl Into<String>) -> Self {
        Self {
            result: Value::Null,
            error_code: Some(code.into()),
            error_info: Some(info.into()),
        }
    }

    /// The envelope as a JSON object.
    pub fn to_value(&self) -> Value {
        json!({
            "result": self.result,
            "error_code": self.error_code,
            "error_info": self.error_info,
        })
    }

    /// The response body PHP would send, keys in PHP's order.
    pub fn to_body(&self) -> String {
        let mut body = String::from("{\"result\":");
        body.push_str(&json::encode(&self.result));
        body.push_str(",\"error_code\":");
        body.push_str(&json::encode(
            &self.error_code.clone().map_or(Value::Null, Value::String),
        ));
        body.push_str(",\"error_info\":");
        body.push_str(&json::encode(
            &self.error_info.clone().map_or(Value::Null, Value::String),
        ));
        body.push('}');
        body
    }
}

/// How [`dispatch()`](crate::call::dispatch) answers a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// Rust answers with this envelope.
    Envelope(Envelope),
    /// PHP must serve this request; the string says why.
    Fallback(&'static str),
}
