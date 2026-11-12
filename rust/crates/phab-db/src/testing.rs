//! Throwaway databases for integration tests.
//!
//! `TestDatabase::create` builds the requested `<namespace>_<application>`
//! databases from `resources/sql/quickstart.sql` under a random namespace,
//! on the server named by `PHAB_TEST_MYSQL_URL` (for example
//! `mysql://root:password@127.0.0.1:3306`). Without that variable the tests
//! are skipped, unless `PHAB_TEST_REQUIRE_MYSQL` is set, as it is in CI.

use std::{fmt::Write as _, path::PathBuf};

use rand::Rng;
use sqlx::{Executor, mysql::MySqlPoolOptions};

use crate::Database;

const CHARSET: &str = "utf8mb4";
const COLLATE_TEXT: &str = "utf8mb4_bin";
const COLLATE_SORT: &str = "utf8mb4_unicode_ci";

pub struct TestDatabase {
    database: Database,
    applications: Vec<String>,
}

fn quickstart_path() -> PathBuf {
    std::env::var_os("PHAB_QUICKSTART_SQL").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../resources/sql/quickstart.sql"),
        PathBuf::from,
    )
}

/// `CREATE TABLE` statements of `application`, with placeholders filled in.
fn table_statements(quickstart: &str, namespace: &str, application: &str) -> Vec<String> {
    let wanted = format!("USE `{{$NAMESPACE}}_{application}`");
    let mut current_matches = false;
    let mut statements = Vec::new();
    for statement in quickstart.split(";\n") {
        let statement = statement.trim();
        if statement.starts_with("USE ") {
            current_matches = statement == wanted;
            continue;
        }
        if current_matches && statement.starts_with("CREATE TABLE") {
            statements.push(
                statement
                    .replace("{$NAMESPACE}", namespace)
                    .replace("{$CHARSET_FULLTEXT}", CHARSET)
                    .replace("{$CHARSET_SORT}", CHARSET)
                    .replace("{$CHARSET}", CHARSET)
                    .replace("{$COLLATE_FULLTEXT}", COLLATE_SORT)
                    .replace("{$COLLATE_SORT}", COLLATE_SORT)
                    .replace("{$COLLATE_TEXT}", COLLATE_TEXT),
            );
        }
    }
    statements
}

impl TestDatabase {
    /// Builds the databases of `applications` (e.g. `["user", "auth"]`).
    /// Returns `None` when no test server is configured.
    pub async fn create(applications: &[&str]) -> Option<Self> {
        let Ok(url) = std::env::var("PHAB_TEST_MYSQL_URL") else {
            assert!(
                std::env::var_os("PHAB_TEST_REQUIRE_MYSQL").is_none(),
                "PHAB_TEST_REQUIRE_MYSQL is set but PHAB_TEST_MYSQL_URL is not"
            );
            eprintln!("skipping: PHAB_TEST_MYSQL_URL is not set");
            return None;
        };
        let quickstart = std::fs::read_to_string(quickstart_path())
            .unwrap_or_else(|error| panic!("reading quickstart.sql: {error}"));

        let suffix: u64 = rand::rng().random();
        let namespace = format!("phabtest{suffix:016x}");
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .unwrap_or_else(|error| panic!("connecting to {url}: {error}"));

        for application in applications {
            let statements = table_statements(&quickstart, &namespace, application);
            assert!(
                !statements.is_empty(),
                "quickstart.sql has no tables for '{application}'"
            );
            let mut script = format!(
                "CREATE DATABASE `{namespace}_{application}` \
                 DEFAULT CHARACTER SET {CHARSET} COLLATE {COLLATE_TEXT};\n\
                 USE `{namespace}_{application}`;\n"
            );
            for statement in statements {
                let _ = writeln!(script, "{statement};");
            }
            pool.execute(script.as_str())
                .await
                .unwrap_or_else(|error| panic!("creating '{application}': {error}"));
        }

        Some(Self {
            database: Database::from_pool(pool, namespace),
            applications: applications.iter().map(|name| (*name).to_owned()).collect(),
        })
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Runs `sql` after replacing `{ns}` with the test namespace.
    pub async fn execute(&self, sql: &str) {
        let sql = sql.replace("{ns}", self.database.namespace());
        self.database
            .pool()
            .execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("running {sql}: {error}"));
    }

    pub async fn drop_databases(self) {
        for application in &self.applications {
            let sql = format!(
                "DROP DATABASE IF EXISTS `{}_{application}`",
                self.database.namespace()
            );
            let _ = self.database.pool().execute(sql.as_str()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tables_of_one_application() {
        let quickstart = "USE `{$NAMESPACE}_a`;\n\nCREATE TABLE `x` (`c` varchar(1) \
             CHARACTER SET {$CHARSET} COLLATE {$COLLATE_TEXT}) ENGINE=InnoDB;\n\n\
             USE `{$NAMESPACE}_b`;\n\nCREATE TABLE `y` (`d` int);\n";
        let statements = table_statements(quickstart, "ns", "a");
        assert_eq!(
            statements,
            [
                "CREATE TABLE `x` (`c` varchar(1) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin) ENGINE=InnoDB"
            ]
        );
        assert_eq!(table_statements(quickstart, "ns", "b").len(), 1);
        assert!(table_statements(quickstart, "ns", "c").is_empty());
    }

    #[test]
    fn real_quickstart_has_the_core_databases() {
        let quickstart = std::fs::read_to_string(quickstart_path()).unwrap();
        for application in ["user", "auth", "conduit", "feed", "differential", "policy"] {
            assert!(
                !table_statements(&quickstart, "ns", application).is_empty(),
                "{application}"
            );
        }
    }
}
