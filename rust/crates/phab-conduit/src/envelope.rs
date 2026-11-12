//! The `{result, error_code, error_info}` response of a call.

use serde_json::{Value, json};

use crate::json;

/// `ConduitAPIResponse`.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub result: Value,
    pub error_code: Option<String>,
    pub error_info: Option<String>,
}

impl Envelope {
    pub fn success(result: Value) -> Self {
        Self {
            result,
            error_code: None,
            error_info: None,
        }
    }

    pub fn error(code: impl Into<String>, info: impl Into<String>) -> Self {
        Self {
            result: Value::Null,
            error_code: Some(code.into()),
            error_info: Some(info.into()),
        }
    }

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

#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Envelope(Envelope),
    /// PHP must serve this request; the string says why.
    Fallback(&'static str),
}
