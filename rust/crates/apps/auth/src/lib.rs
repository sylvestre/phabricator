//! Methods of `PhabricatorAuthApplication`. Those which write are
//! registered only when Rust logs users in (`PHAB_RUST_LOGIN=1`); otherwise
//! they stay with PHP.

pub mod public_keys;

use phab_auth::{Writes, session};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, Registry};
use serde_json::Value;

/// `PhabricatorAuthApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorAuthApplication",
    name: "Auth",
};

/// `PhabricatorAuthLogoutConduitAPIMethod`: ends every login session of
/// the caller, web and Conduit alike, as `terminateLoginSessions()` does.
pub struct AuthLogout;

impl Method for AuthLogout {
    fn name(&self) -> &'static str {
        "auth.logout"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(user) = &request.user else {
                return Err(MethodError::Unsupported("auth.logout without a user"));
            };
            // PHP fails to write on a read-only install.
            if request.context.config.is_read_only() {
                return Err(MethodError::Unsupported("a read-only install"));
            }
            // Shadow calls answer without writing.
            if request.writes == Writes::Allowed {
                session::terminate_login_sessions(&request.context.database, &user.phid)
                    .await
                    .map_err(MethodError::internal)?;
            }
            Ok(Value::Null)
        })
    }
}

/// Registers the Auth methods which write, for when Rust logs users in.
pub fn register(registry: &mut Registry) {
    registry.register(AuthLogout);
}

/// Registers the read-only Auth methods.
pub fn register_queries(registry: &mut Registry) {
    registry.register(public_keys::QueryPublicKeys);
}
