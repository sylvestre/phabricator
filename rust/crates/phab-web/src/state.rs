//! What the pages need from the service.

use phab_core::Config;
use phab_db::Database;

/// The service state the web routes read. `phab-server` implements it for
/// its own state, so pages share the configuration and the connections of
/// the Conduit methods.
pub trait WebState: Send + Sync + 'static {
    /// The database connections.
    fn database(&self) -> &Database;
    /// The Phabricator configuration.
    fn config(&self) -> &Config;
    /// The current Unix time in seconds (`PhabricatorTime::getNow()`).
    fn now(&self) -> i64;
    /// The Remarkup engine, built once from the configuration
    /// ([`crate::remarkup::new_engine()`]).
    fn remarkup(&self) -> &phab_remarkup::Engine;
}
