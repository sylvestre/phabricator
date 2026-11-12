//! What the service needs before it can answer: configuration and the
//! database.

use std::path::{Path, PathBuf};

use phab_conduit::Context;
use phab_core::Config;
use phab_db::Database;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Config(#[from] phab_core::config::ConfigError),
    #[error(transparent)]
    Database(#[from] phab_db::DbError),
}

/// Builds the configuration stack like `PhabricatorEnv` (defaults,
/// `conf/local/local.json`, then the database) and connects.
pub async fn build_context(config_dir: &Path, hostname: String) -> Result<Context, StartupError> {
    let mut config = Config::default();
    config.push_local_file(&config_dir.join("local.json"))?;
    let database = Database::connect(&config).await?;
    let entries = database.load_config_entries().await?;
    config.push_layer("Database", entries);
    Ok(Context {
        database,
        config,
        hostname,
    })
}

/// `php_uname('n')`.
pub fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

pub fn default_config_dir() -> PathBuf {
    PathBuf::from("/app/phabricator/conf/local")
}
