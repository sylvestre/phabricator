//! Methods of `PhabricatorConduitApplication`.

use phab_conduit::{ApiRequest, Application, Method, MethodFuture, Registry};
use serde_json::{Value, json};

/// `PhabricatorConduitApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorConduitApplication",
    name: "Conduit",
};

/// `ConduitPingConduitAPIMethod`: the host name, for health checks.
pub struct ConduitPing;

impl Method for ConduitPing {
    fn name(&self) -> &'static str {
        "conduit.ping"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn requires_authentication(&self) -> bool {
        false
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move { Ok(Value::String(request.context.hostname.clone())) })
    }
}

/// `ConduitGetCapabilitiesConduitAPIMethod`: the protocols and formats
/// this server speaks.
pub struct ConduitGetCapabilities;

impl Method for ConduitGetCapabilities {
    fn name(&self) -> &'static str {
        "conduit.getcapabilities"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn requires_authentication(&self) -> bool {
        false
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, _request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            Ok(json!({
                "authentication": ["token", "asymmetric", "session", "sessionless"],
                "signatures": ["consign"],
                "input": ["json", "urlencoded"],
                "output": ["json", "human"],
            }))
        })
    }
}

/// Registers the Conduit methods.
pub fn register(registry: &mut Registry) {
    registry.register(ConduitGetCapabilities);
    registry.register(ConduitPing);
}
