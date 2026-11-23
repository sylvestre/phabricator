//! The workflows of `phab-cli`, against a temporary `local.json` and, when
//! `PHAB_TEST_MYSQL_URL` is set, a test database.

use std::path::Path;

use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const NOW: i64 = 1_700_000_000;

/// What a workflow printed, and its exit status.
struct Ran {
    status: u8,
    out: String,
    err: String,
}

async fn run(config_dir: &Path, arguments: &[&str]) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut context = phab_cli::Context {
        config_dir: config_dir.to_owned(),
        out: &mut out,
        err: &mut err,
        ansi: false,
        now: NOW,
    };
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
    let status = phab_cli::run(&mut context, &arguments).await;
    Ran {
        status,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn write_local(directory: &Path, values: &Value) {
    std::fs::write(
        directory.join("local.json"),
        serde_json::to_string_pretty(values).unwrap(),
    )
    .unwrap();
}

fn read_local(directory: &Path) -> String {
    std::fs::read_to_string(directory.join("local.json")).unwrap()
}

/// A `local.json` whose database can't be reached, quickly.
fn unreachable_database() -> Value {
    json!({"mysql.host": "127.0.0.1", "mysql.port": "1"})
}

#[tokio::test]
async fn list_prints_every_key() {
    let directory = tempfile::tempdir().unwrap();
    let ran = run(directory.path(), &["config", "list"]).await;
    assert_eq!(ran.status, 0);
    let keys: Vec<&str> = ran.out.lines().collect();
    assert!(keys.contains(&"phabricator.base-uri"));
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
}

#[tokio::test]
async fn local_set_get_and_delete_rewrite_local_json_as_php_does() {
    let directory = tempfile::tempdir().unwrap();
    write_local(directory.path(), &unreachable_database());

    let ran = run(
        directory.path(),
        &["config", "set", "phabricator.show-prototypes", "true"],
    )
    .await;
    assert_eq!(ran.status, 0, "{}", ran.err);
    assert_eq!(
        ran.out,
        format!(
            " DONE  Wrote configuration key \"phabricator.show-prototypes\" to local storage (in \
             file \"{}\").\n",
            directory.path().join("local.json").display()
        )
    );
    assert_eq!(
        read_local(directory.path()),
        "{\n  \"phabricator.show-prototypes\": true,\n  \"mysql.host\": \"127.0.0.1\",\n  \
         \"mysql.port\": \"1\"\n}\n"
    );

    let ran = run(
        directory.path(),
        &["config", "get", "phabricator.show-prototypes"],
    )
    .await;
    assert_eq!(ran.status, 0);
    assert_eq!(
        ran.out,
        "{\n  \"config\": [\n    {\n      \"key\": \"phabricator.show-prototypes\",\n      \
         \"source\": \"local\",\n      \"value\": true,\n      \"status\": \"set\",\n      \
         \"errorInfo\": null\n    },\n    {\n      \"key\": \"phabricator.show-prototypes\",\n      \
         \"source\": \"database\",\n      \"value\": null,\n      \"status\": \"error\",\n      \
         \"errorInfo\": \"Database source is not configured properly\"\n    }\n  ]\n}\n"
    );

    let ran = run(
        directory.path(),
        &["config", "delete", "phabricator.show-prototypes"],
    )
    .await;
    assert_eq!(ran.status, 0);
    assert_eq!(
        ran.out,
        "Deleted 'phabricator.show-prototypes' from local configuration.\n"
    );
    let ran = run(
        directory.path(),
        &["config", "delete", "phabricator.show-prototypes"],
    )
    .await;
    assert_eq!(ran.status, phab_cli::EXIT_USAGE);
    assert_eq!(
        ran.err,
        "Usage Exception: Configuration key 'phabricator.show-prototypes' is not set in local \
         configuration!\n"
    );
}

#[tokio::test]
async fn set_refuses_what_php_refuses_and_leaves_what_it_does_not_port() {
    let directory = tempfile::tempdir().unwrap();
    write_local(directory.path(), &json!({}));
    let before = read_local(directory.path());
    for (arguments, status) in [
        (
            &["config", "set", "no.such-key", "1"][..],
            phab_cli::EXIT_USAGE,
        ),
        (
            &["config", "set", "phabricator.show-prototypes", "yes"],
            phab_cli::EXIT_USAGE,
        ),
        (
            &["config", "set", "phabricator.base-uri", "http://example/"],
            phab_cli::EXIT_USAGE,
        ),
        (
            &["config", "set", "phabricator.base-uri"],
            phab_cli::EXIT_USAGE,
        ),
        (
            &["config", "set", "policy.locked", "{}"],
            phab_cli::EXIT_LEFT_TO_PHP,
        ),
        (
            &["config", "set", "cluster.mailers", "[]"],
            phab_cli::EXIT_LEFT_TO_PHP,
        ),
        (&["config", "get", "no.such-key"], phab_cli::EXIT_USAGE),
        (&["config", "migrate"], phab_cli::EXIT_LEFT_TO_PHP),
    ] {
        let ran = run(directory.path(), arguments).await;
        assert_eq!(ran.status, status, "{arguments:?}: {}", ran.err);
    }
    assert_eq!(read_local(directory.path()), before);
    let ran = run(directory.path(), &["config", "set", "no.such-key", "1"]).await;
    assert_eq!(
        ran.err,
        "Usage Exception: Configuration key \"no.such-key\" is unknown. Use \"bin/config list\" \
         to list all known keys.\n"
    );
}

/// `local.json` pointing at the test database.
fn local_for(test_db: &TestDatabase) -> Value {
    let url = std::env::var("PHAB_TEST_MYSQL_URL").unwrap();
    let rest = url.strip_prefix("mysql://").unwrap();
    let (credentials, address) = rest.rsplit_once('@').unwrap();
    let (user, password) = credentials.split_once(':').unwrap_or((credentials, ""));
    let address = address.trim_end_matches('/');
    let (host, port) = address.split_once(':').unwrap_or((address, "3306"));
    json!({
        "mysql.host": host,
        "mysql.port": port,
        "mysql.user": user,
        "mysql.pass": password,
        "storage.default-namespace": test_db.database().namespace(),
    })
}

#[tokio::test]
async fn database_set_get_and_delete_write_config_entry() {
    let Some(test_db) = TestDatabase::create(&["config"]).await else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    write_local(directory.path(), &local_for(&test_db));
    let database = test_db.database();

    let ran = run(
        directory.path(),
        &[
            "config",
            "set",
            "--database",
            "auth.email-domains",
            "[\"https://a/é\"]",
        ],
    )
    .await;
    assert_eq!(ran.status, 0, "{}", ran.err);
    assert_eq!(
        ran.out,
        " DONE  Wrote configuration key \"auth.email-domains\" to database storage.\n"
    );
    let row = phab_db::sqlx::query_as::<_, (String, String, i8, u32, u32)>(&format!(
        "SELECT CAST(phid AS CHAR), CAST(value AS CHAR), isDeleted, dateCreated, dateModified FROM {} \
         WHERE configKey = 'auth.email-domains' AND namespace = 'default'",
        database.table("config", "config_entry")
    ))
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert!(row.0.starts_with("PHID-CONF-"), "{}", row.0);
    assert_eq!(row.1, "[\"https:\\/\\/a\\/\\u00e9\"]");
    assert_eq!((row.2, i64::from(row.3), i64::from(row.4)), (0, NOW, NOW));

    let ran = run(directory.path(), &["config", "get", "auth.email-domains"]).await;
    assert!(
        ran.out.contains(
            "\"source\": \"database\",\n      \"value\": [\n        \"https://a/\\u00e9\"\n      \
             ],\n      \"status\": \"set\""
        ),
        "{}",
        ran.out
    );

    // Locked options can only be set locally.
    let ran = run(
        directory.path(),
        &[
            "config",
            "set",
            "--database",
            "phabricator.base-uri",
            "http://a.b/",
        ],
    )
    .await;
    assert_eq!(ran.status, phab_cli::EXIT_USAGE);

    let ran = run(
        directory.path(),
        &["config", "delete", "--database", "auth.email-domains"],
    )
    .await;
    assert_eq!(ran.status, 0, "{}", ran.err);
    assert_eq!(
        ran.out,
        "Deleted 'auth.email-domains' from database configuration.\n"
    );
    let deleted: i8 = phab_db::sqlx::query_scalar(&format!(
        "SELECT isDeleted FROM {} WHERE configKey = 'auth.email-domains'",
        database.table("config", "config_entry")
    ))
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(deleted, 1);
    let ran = run(
        directory.path(),
        &["config", "delete", "--database", "auth.email-domains"],
    )
    .await;
    assert_eq!(ran.status, phab_cli::EXIT_USAGE);

    // A deleted entry comes back.
    let ran = run(
        directory.path(),
        &["config", "set", "--database", "auth.email-domains", "[]"],
    )
    .await;
    assert_eq!(ran.status, 0, "{}", ran.err);
    let count: i64 = phab_db::sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE isDeleted = 0",
        database.table("config", "config_entry")
    ))
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn storage_status_lists_the_patches_and_their_state() {
    let Some(test_db) = TestDatabase::create(&["meta_data", "config"]).await else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("phabricator");
    let config_dir = root.join("conf/local");
    std::fs::create_dir_all(&config_dir).unwrap();
    write_local(&config_dir, &local_for(&test_db));
    test_db
        .execute(
            "DELETE FROM `{ns}_meta_data`.patch_status; \
             INSERT INTO `{ns}_meta_data`.patch_status (patch, applied, duration) VALUES \
             ('phabricator:db.audit', 1, NULL), \
             ('phabricator:20190412.herald.01.rebuild.php', 1, 4915)",
        )
        .await;
    let ran = run(&config_dir, &["storage", "status"]).await;
    assert_eq!(ran.status, 0, "{}", ran.err);
    let lines: Vec<&str> = ran.out.lines().collect();
    assert_eq!(lines.len(), phab_cli::storage::patches().len());
    let squeeze = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    let local = local_for(&test_db);
    let host = format!(
        "{}:{}",
        local["mysql.host"].as_str().unwrap(),
        local["mysql.port"].as_str().unwrap()
    );
    assert_eq!(
        squeeze(lines[0]),
        format!("phabricator:db.audit default {host} Applied - db audit")
    );
    let herald = lines
        .iter()
        .find(|line| line.starts_with("phabricator:20190412.herald.01.rebuild.php "))
        .unwrap();
    assert_eq!(
        squeeze(herald),
        format!(
            "phabricator:20190412.herald.01.rebuild.php worker {host} Applied 4,915 us php {}/\
             resources/sql/autopatches/20190412.herald.01.rebuild.php",
            root.display()
        )
    );
    assert!(lines.iter().any(|line| line.contains(" Not Applied ")));
}

#[tokio::test]
async fn worker_execute_leaves_classes_it_does_not_run_to_php() {
    let Some(test_db) = TestDatabase::create(&["worker", "config"]).await else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    write_local(directory.path(), &local_for(&test_db));
    test_db
        .execute(
            "INSERT INTO `{ns}_worker`.worker_taskdata (id, data) VALUES (1, '[]'); \
             INSERT INTO `{ns}_worker`.worker_activetask (id, taskClass, leaseOwner, \
               leaseExpires, failureCount, dataID, priority, dateCreated, dateModified) VALUES \
             (7, 'SomePHPOnlyWorker', 'php:1', 1800000000, 0, 1, 2000, 1, 1); \
             INSERT INTO `{ns}_worker`.worker_archivetask (id, taskClass, leaseOwner, \
               leaseExpires, failureCount, dataID, priority, result, duration, dateCreated, \
               dateModified, archivedEpoch) VALUES \
             (8, 'FeedPublisherWorker', NULL, NULL, 0, 1, 2000, 0, 10, 1, 1, 1)",
        )
        .await;
    let ran = run(directory.path(), &["worker", "execute", "--id", "7"]).await;
    assert_eq!(ran.status, phab_cli::EXIT_LEFT_TO_PHP, "{}", ran.err);
    // Nothing was written: the lease is PHP's still.
    let owner: Option<String> = phab_db::sqlx::query_scalar(&format!(
        "SELECT CAST(leaseOwner AS CHAR) FROM {} WHERE id = 7",
        test_db.database().table("worker", "worker_activetask")
    ))
    .fetch_one(test_db.database().pool())
    .await
    .unwrap();
    assert_eq!(owner.as_deref(), Some("php:1"));

    let ran = run(directory.path(), &["worker", "execute", "--id", "8"]).await;
    assert_eq!(ran.status, 0);
    assert!(
        ran.err
            .contains(" ARCHIVED  Task 8 (FeedPublisherWorker) is already archived")
    );
    assert!(ran.err.contains(" NO TASKS  No tasks selected to execute."));

    let ran = run(directory.path(), &["worker", "execute", "--id", "9"]).await;
    assert_eq!(ran.status, phab_cli::EXIT_USAGE);
    let ran = run(directory.path(), &["worker", "execute", "--class", "X"]).await;
    assert_eq!(ran.status, phab_cli::EXIT_LEFT_TO_PHP);
}

#[tokio::test]
async fn repository_needs_a_workflow_and_a_repository() {
    let directory = tempfile::tempdir().unwrap();
    let ran = run(directory.path(), &["repository", "pull", "R1"]).await;
    assert_eq!(ran.status, phab_cli::EXIT_USAGE);
    let ran = run(directory.path(), &["nope"]).await;
    assert_eq!(ran.status, phab_cli::EXIT_USAGE);
}
