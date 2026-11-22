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
    /// PHP's Conduit API, which writes go to until the editor is ported.
    fn php(&self) -> Option<&crate::php::PhpConduit>;
    /// The Searchfox trees of repositories, from `PHAB_SEARCHFOX_TREES`.
    fn searchfox_trees(&self) -> &crate::searchfox::Trees {
        crate::searchfox::Trees::from_env()
    }
    /// The web login settings, present only when Rust logs users in
    /// (`PHAB_RUST_LOGIN=1`); without them the login pages go to PHP.
    fn login(&self) -> Option<&crate::login::LoginSettings> {
        None
    }
}
