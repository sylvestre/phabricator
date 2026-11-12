//! What a call runs with: the service's context and the HTTP request.

use phab_core::Config;
use phab_db::Database;

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
