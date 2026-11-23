//! `PhabricatorGlobalLock`: the MySQL named locks PHP's repository engines
//! take, so PHP and Rust never work on a repository at the same time.

use phab_core::hash::digest_to_length;
use phab_db::Database;
use sqlx::MySqlConnection;
use sqlx::pool::PoolConnection;

use crate::import::ImportError;

/// `PhabricatorGlobalLock::newLock($name, array('repositoryPHID' => ...))`:
/// `ph:<namespace digest>:<lock digest>`.
pub fn repository_lock_name(namespace: &str, name: &str, repository_phid: &str) -> String {
    let digest = |text: &str| {
        let bytes = digest_to_length(text.as_bytes(), 20).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    // `phutil_json_encode()` of the PHID, which has nothing to escape.
    let local = format!("{name}(repositoryPHID=\"{repository_phid}\")");
    format!("ph:{}:{}", digest(namespace), digest(&local))
}

/// A held lock, on its own connection.
pub struct GlobalLock {
    connection: PoolConnection<sqlx::MySql>,
    name: String,
}

impl GlobalLock {
    /// `lock()` without waiting; `None` when another process holds it.
    pub async fn try_lock(database: &Database, name: String) -> Result<Option<Self>, ImportError> {
        let mut connection = database.pool().acquire().await?;
        let held: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, 0)")
            .bind(&name)
            .fetch_one(&mut *connection)
            .await?;
        if held != Some(1) {
            return Ok(None);
        }
        Ok(Some(Self { connection, name }))
    }

    /// `unlock()`.
    pub async fn unlock(mut self) -> Result<(), ImportError> {
        release(&mut self.connection, &self.name).await
    }
}

async fn release(connection: &mut MySqlConnection, name: &str) -> Result<(), ImportError> {
    sqlx::query("SELECT RELEASE_LOCK(?)")
        .bind(name)
        .execute(connection)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_locks_like_php() {
        let name = repository_lock_name("phabricator", "repo.look", "PHID-REPO-x");
        let parts: Vec<&str> = name.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "ph");
        assert_eq!(parts[1].len(), 20);
        assert_eq!(parts[2].len(), 20);
    }
}
