//! The Conduit API framework: what `PhabricatorConduitAPIController` and
//! `ConduitCall` do around each method.
//!
//! [`call::dispatch`] decodes a request, authenticates it, runs the method
//! and wraps the result in the `{result, error_code, error_info}` envelope.
//! Whenever the Rust side can't answer exactly like PHP (a method or an
//! authentication mechanism that is not ported yet), it returns
//! [`Response::Fallback`] and the HTTP layer hands the request to PHP.

pub mod call;
pub mod form;
pub mod json;
pub mod request;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use phab_auth::{User, Writes};
use phab_core::Config;
use phab_db::Database;
use phab_policy::Viewer;
use serde_json::{Map, Value, json};

/// What a method needs from the running service.
pub struct Context {
    pub database: Database,
    pub config: Config,
    /// `php_uname('n')`.
    pub hostname: String,
}

/// How the request is being served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The answer goes to the client.
    Serve,
    /// PHP answers the client; Rust only computes its answer to compare it.
    /// Nothing is written to the database.
    Shadow,
}

/// The parts of an HTTP request Conduit looks at.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    pub content_type: Option<String>,
    /// Raw query string, without the leading `?`.
    pub query: String,
    /// Raw request body.
    pub body: String,
    /// Value of the `phsid` cookie, if any.
    pub session_cookie: Option<String>,
}

/// The decoded parameters of a call, as `ConduitAPIRequest` holds them.
pub struct ApiRequest<'a> {
    pub context: &'a Context,
    /// `None` for methods which don't require authentication.
    pub user: Option<User>,
    pub viewer: Viewer,
    pub params: Map<String, Value>,
    /// False when parameters came as plain form fields, which are strings.
    pub strictly_typed: bool,
    pub now: i64,
    pub writes: Writes,
}

impl ApiRequest<'_> {
    /// `ConduitAPIRequest::getValue()`.
    pub fn value(&self, key: &str) -> Option<&Value> {
        self.params.get(key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodError {
    /// A `ConduitException`: `error_code` is `code`, `error_info` the given
    /// description or the method's description of the code.
    Conduit { code: String, info: Option<String> },
    /// The answer depends on something not ported; PHP must answer.
    Unsupported(&'static str),
    /// An unexpected failure (database error...).
    Internal(String),
}

impl MethodError {
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::Internal(error.to_string())
    }
}

/// Unexpected failures of the layers methods are built on, so methods can
/// use `?` on them.
macro_rules! internal_from {
    ($($error:ty),* $(,)?) => {$(
        impl From<$error> for MethodError {
            fn from(error: $error) -> Self {
                Self::internal(error)
            }
        }
    )*};
}

internal_from!(
    phab_db::DbError,
    phab_db::sqlx::Error,
    phab_policy::loader::LoaderError,
    phab_policy::PolicyError,
    phab_core::config::ConfigError,
);

pub type MethodFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, MethodError>> + Send + 'a>>;

/// The application a method belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Application {
    pub class: &'static str,
    pub name: &'static str,
}

/// A Conduit method (`ConduitAPIMethod`).
pub trait Method: Send + Sync {
    fn name(&self) -> &'static str;

    /// Names of the parameters from `defineParamTypes()`.
    fn params(&self) -> &'static [&'static str];

    fn requires_authentication(&self) -> bool {
        true
    }

    fn allows_public(&self) -> bool {
        false
    }

    fn application(&self) -> Option<Application>;

    /// `defineErrorTypes()`.
    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a>;
}

/// The methods the Rust service implements.
#[derive(Default, Clone)]
pub struct Registry {
    methods: HashMap<&'static str, Arc<dyn Method>>,
}

impl Registry {
    pub fn register(&mut self, method: impl Method + 'static) {
        self.methods.insert(method.name(), Arc::new(method));
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Method>> {
        self.methods.get(name)
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.methods.keys().copied().collect();
        names.sort_unstable();
        names
    }
}

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
