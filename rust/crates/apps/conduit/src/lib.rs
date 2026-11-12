//! Methods of `PhabricatorConduitApplication`.

use phab_conduit::{ApiRequest, Application, Method, MethodFuture, Registry};
use serde_json::Value;

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

pub fn register(registry: &mut Registry) {
    registry.register(ConduitPing);
}
