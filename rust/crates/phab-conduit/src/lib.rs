//! The Conduit API framework: what `PhabricatorConduitAPIController` and
//! `ConduitCall` do around each method.
//!
//! [`call::dispatch`] decodes a request, authenticates it, runs the method
//! and wraps the result in the `{result, error_code, error_info}` envelope.
//! Whenever the Rust side can't answer exactly like PHP (a method or an
//! authentication mechanism that is not ported yet), it returns
//! [`Response::Fallback`] and the HTTP layer hands the request to PHP.

pub mod call;
mod context;
pub mod edit;
mod envelope;
pub mod form;
pub mod json;
mod method;
pub mod order;
pub mod pager;
pub mod request;
pub mod search;

pub use context::{Context, HttpRequest, Mode};
pub use envelope::{Envelope, Response};
pub use method::{ApiRequest, Application, Method, MethodError, MethodFuture, Registry};
