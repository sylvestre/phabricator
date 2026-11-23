//! `PhabricatorGlobalLock`: the MySQL named locks the engine takes on a
//! build (`harbormaster.build:<id>`) and on a buildable
//! (`harbormaster.buildable:<id>`), so PHP and Rust never update one at the
//! same time.

use phab_core::hash::digest_to_length;
use phab_db::Database;
use phab_db::sqlx::pool::PoolConnection;
use phab_db::sqlx::{self, MySql};

use super::EngineError;

/// How long the engine waits for its locks, in seconds (`lock(15)`).
const WAIT: i64 = 15;

/// `PhabricatorGlobalLock::newLock($name)` without parameters:
/// `ph:<namespace digest>:<lock digest>`.
pub(super) fn lock_name(namespace: &str, name: &str) -> String {
    let digest = |text: &str| {
        let bytes = digest_to_length(text.as_bytes(), 20).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    format!("ph:{}:{}", digest(namespace), digest(&format!("{name}()")))
}

/// A held lock, on its own connection.
pub(super) struct GlobalLock {
    connection: PoolConnection<MySql>,
    name: String,
}

impl GlobalLock {
    /// `lock(15)`: a `PhutilLockException` when another process keeps it.
    pub(super) async fn lock(database: &Database, name: &str) -> Result<Self, EngineError> {
        let name = lock_name(database.namespace(), name);
        let mut connection = database.pool().acquire().await?;
        let held: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, ?)")
            .bind(&name)
            .bind(WAIT)
            .fetch_one(&mut *connection)
            .await?;
        if held != Some(1) {
            return Err(EngineError::Failed(format!(
                "Failed to acquire lock \"{name}\"."
            )));
        }
        Ok(Self { connection, name })
    }

    /// `unlock()`.
    pub(super) async fn unlock(mut self) -> Result<(), EngineError> {
        sqlx::query("SELECT RELEASE_LOCK(?)")
            .bind(&self.name)
            .execute(&mut *self.connection)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_locks_like_php() {
        let name = lock_name("phabricator", "harbormaster.build:1");
        let parts: Vec<&str> = name.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "ph");
        assert_eq!(parts[1].len(), 20);
        assert_eq!(parts[2].len(), 20);
        assert_ne!(name, lock_name("phabricator", "harbormaster.build:2"));
    }
}
