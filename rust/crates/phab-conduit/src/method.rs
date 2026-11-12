//! Conduit methods (`ConduitAPIMethod`) and the registry of ported ones.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use phab_auth::{User, Writes};
use phab_policy::Viewer;
use serde_json::{Map, Value};

use crate::Context;

/// The decoded parameters of a call, as `ConduitAPIRequest` holds them.
pub struct ApiRequest<'a> {
    /// The service the call runs in.
    pub context: &'a Context,
    /// `None` for methods which don't require authentication.
    pub user: Option<User>,
    /// Who policies are checked for; anonymous without authentication.
    pub viewer: Viewer,
    /// The parameters, without the `__conduit__` metadata.
    pub params: Map<String, Value>,
    /// False when parameters came as plain form fields, which are strings.
    pub strictly_typed: bool,
    /// The request time (epoch seconds).
    pub now: i64,
    /// Whether the method may write to the database.
    pub writes: Writes,
}

impl ApiRequest<'_> {
    /// `ConduitAPIRequest::getValue()`.
    pub fn value(&self, key: &str) -> Option<&Value> {
        self.params.get(key)
    }
}

/// Why a method returned no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodError {
    /// A `ConduitException`: `error_code` is `code`, `error_info` the given
    /// description or the method's description of the code.
    Conduit {
        /// The `error_code`.
        code: String,
        /// The `error_info`; `None` for the method's description of `code`.
        info: Option<String>,
    },
    /// The answer depends on something not ported; PHP must answer.
    Unsupported(&'static str),
    /// An unexpected failure (database error...).
    Internal(String),
}

impl MethodError {
    /// Wraps an unexpected failure as [`MethodError::Internal`].
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

/// The future [`Method::execute()`] returns.
pub type MethodFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, MethodError>> + Send + 'a>>;

/// The application a method belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Application {
    /// The PHP class of the application.
    pub class: &'static str,
    /// The application name, for error messages.
    pub name: &'static str,
}

/// A Conduit method (`ConduitAPIMethod`).
pub trait Method: Send + Sync {
    /// `getAPIMethodName()`.
    fn name(&self) -> &'static str;

    /// Names of the parameters from `defineParamTypes()`.
    fn params(&self) -> &'static [&'static str];

    /// `shouldRequireAuthentication()`.
    fn requires_authentication(&self) -> bool {
        true
    }

    /// `shouldAllowPublic()`.
    fn allows_public(&self) -> bool {
        false
    }

    /// `getApplication()`: the method is disabled when it is not installed.
    fn application(&self) -> Option<Application>;

    /// `defineErrorTypes()`.
    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// `execute()`.
    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a>;
}

/// The methods the Rust service implements.
#[derive(Default, Clone)]
pub struct Registry {
    methods: HashMap<&'static str, Arc<dyn Method>>,
}

impl Registry {
    /// Adds `method`, replacing a method of the same name.
    pub fn register(&mut self, method: impl Method + 'static) {
        self.methods.insert(method.name(), Arc::new(method));
    }

    /// The method called `name`, if it is ported.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Method>> {
        self.methods.get(name)
    }

    /// The names of the registered methods, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.methods.keys().copied().collect();
        names.sort_unstable();
        names
    }
}
