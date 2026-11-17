//! The `phid.*` Conduit methods: what the viewer can see of objects, by
//! PHID or by name.

pub mod handles;
pub mod methods;

use phab_conduit::Registry;

/// Registers the `phid.*` methods.
pub fn register(registry: &mut Registry) {
    registry.register(methods::PhidQuery);
    registry.register(methods::PhidInfo);
    registry.register(methods::PhidLookup);
}
