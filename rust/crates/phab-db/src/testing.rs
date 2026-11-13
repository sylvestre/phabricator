//! Throwaway databases for integration tests.
//!
//! `TestDatabase::create` builds the requested `<namespace>_<application>`
//! databases from `resources/sql/quickstart.sql` under a random namespace,
//! on the server named by `PHAB_TEST_MYSQL_URL` (for example
//! `mysql://root:password@127.0.0.1:3306`). Without that variable the tests
//! are skipped, unless `PHAB_TEST_REQUIRE_MYSQL` is set, as it is in CI.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rand::Rng;
use sqlx::Executor;
use sqlx::mysql::MySqlPoolOptions;

use crate::Database;

const CHARSET: &str = "utf8mb4";
const COLLATE_TEXT: &str = "utf8mb4_bin";
const COLLATE_SORT: &str = "utf8mb4_unicode_ci";

/// Databases created for one test, under a random namespace.
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

/// Fills the placeholders `bin/storage` replaces in schema files.
fn fill_placeholders(statement: &str, namespace: &str) -> String {
    statement
        .replace("{$NAMESPACE}", namespace)
        .replace("{$CHARSET_FULLTEXT}", CHARSET)
        .replace("{$CHARSET_SORT}", CHARSET)
        .replace("{$CHARSET}", CHARSET)
        .replace("{$COLLATE_FULLTEXT}", COLLATE_SORT)
        .replace("{$COLLATE_SORT}", COLLATE_SORT)
        .replace("{$COLLATE_TEXT}", COLLATE_TEXT)
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
            statements.push(fill_placeholders(statement, namespace));
        }
    }
    statements
}

/// A schema patch: its file name and its SQL.
struct Patch {
    name: String,
    sql: String,
}

/// The `.sql` autopatches newer than `quickstart.sql`, which is a dump that
/// records the patches it contains in `patch_status`. They come sorted by
/// name, the order `bin/storage upgrade` applies them in. The `.php` patches
/// migrate data and are skipped.
fn pending_patches(quickstart: &str, directory: &Path) -> Vec<Patch> {
    let applied: HashSet<&str> = quickstart
        .split("('phabricator:")
        .skip(1)
        .filter_map(|rest| rest.split_once('\'').map(|(name, _)| name))
        .collect();
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()));
    let mut patches: Vec<Patch> = entries
        .map(|entry| entry.expect("listing the autopatches").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sql"))
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            if applied.contains(name.as_str()) {
                return None;
            }
            let sql = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
            Some(Patch { name, sql })
        })
        .collect();
    patches.sort_by(|left, right| left.name.cmp(&right.name));
    patches
}

/// The statements of `patch`, or `None` if it touches a database outside
/// `applications`.
fn patch_statements(patch: &Patch, namespace: &str, applications: &[&str]) -> Option<Vec<String>> {
    let touches_others = patch
        .sql
        .split("{$NAMESPACE}_")
        .skip(1)
        .map(|rest| {
            rest.split(|character: char| !character.is_ascii_alphanumeric())
                .next()
        })
        .any(|application| !application.is_some_and(|name| applications.contains(&name)));
    if touches_others {
        return None;
    }
    Some(
        patch
            .sql
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
            .map(|statement| fill_placeholders(statement, namespace))
            .collect(),
    )
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

        let autopatches = quickstart_path().with_file_name("autopatches");
        for patch in pending_patches(&quickstart, &autopatches) {
            let Some(statements) = patch_statements(&patch, &namespace, applications) else {
                continue;
            };
            for statement in statements {
                pool.execute(statement.as_str())
                    .await
                    .unwrap_or_else(|error| panic!("applying {}: {error}", patch.name));
            }
        }

        Some(Self {
            database: Database::from_pool(pool, namespace),
            applications: applications.iter().map(|name| (*name).to_owned()).collect(),
        })
    }

    /// The connection to the test databases.
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

    /// Drops the databases created by [`TestDatabase::create`].
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
        assert_eq!(
            table_statements(quickstart, "ns", "c"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn selects_patches_of_the_created_databases() {
        let patch = |sql: &str| Patch {
            name: "p.sql".to_owned(),
            sql: sql.to_owned(),
        };
        assert_eq!(
            patch_statements(
                &patch(
                    "ALTER TABLE {$NAMESPACE}_user.user ADD x INT;\nUPDATE {$NAMESPACE}_user.user SET x = 1;\n"
                ),
                "ns",
                &["user"]
            ),
            Some(vec![
                "ALTER TABLE ns_user.user ADD x INT".to_owned(),
                "UPDATE ns_user.user SET x = 1".to_owned()
            ])
        );
        assert_eq!(
            patch_statements(&patch("DROP TABLE {$NAMESPACE}_search.x;"), "ns", &["user"]),
            None
        );
    }

    #[test]
    fn finds_the_patches_newer_than_quickstart() {
        let quickstart = std::fs::read_to_string(quickstart_path()).unwrap();
        let names: Vec<String> = pending_patches(
            &quickstart,
            &quickstart_path().with_file_name("autopatches"),
        )
        .into_iter()
        .map(|patch| patch.name)
        .collect();
        assert!(
            names.contains(&"20200220.xaccount.01.sql".to_owned()),
            "{names:?}"
        );
        assert!(!names.contains(&"20190412.dashboard.01.panels.php".to_owned()));
        assert!(
            !names
                .iter()
                .any(|name| name == "20180418.alamanc.interface.unique.php")
        );
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
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
