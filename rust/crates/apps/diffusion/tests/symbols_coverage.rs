//! `diffusion.findsymbols` and `diffusion.updatecoverage` against a test
//! database.
//!
//! Alice calls. Repository 1 (Git, callsign `ONE`) and repository 2 (Git,
//! short name `two`) are visible to users; repository 3 only to admins.

// One fixture, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-sysysysysysysysysysysysysysy";
const NOW: i64 = 1_700_000_000;

const SEED: &str = "\
    INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
      conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
      accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
    ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
    INSERT INTO `{ns}_conduit`.conduit_token \
      (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
    ('PHID-USER-alice', 'api', 'TOKEN', NULL, 1, 1); \
    INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, repositorySlug, \
      versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
      pushPolicy, localPath) VALUES \
    (1, 'PHID-REPO-one', 'One', 'ONE', NULL, 'git', '{\"default-branch\":\"main/x\"}', 1, 1, \
      'users', 'users', 'users', '/var/repo/1/'), \
    (2, 'PHID-REPO-two', 'Two', NULL, 'two', 'git', '{}', 1, 1, 'users', 'users', \
      'users', '/var/repo/2/'), \
    (3, 'PHID-REPO-three', 'Three', 'THREE', NULL, 'git', '{}', 1, 1, 'admin', 'admin', \
      'admin', '/var/repo/3/'); \
    INSERT INTO `{ns}_repository`.repository_path (id, path, pathHash) VALUES \
    (1, '/src/a b.c', MD5('/src/a b.c')), (2, '/README', MD5('/README')); \
    INSERT INTO `{ns}_repository`.repository_symbol (repositoryPHID, symbolContext, \
      symbolName, symbolType, symbolLanguage, pathID, lineNumber) VALUES \
    ('PHID-REPO-one', 'Foo', 'run', 'function', 'php', 1, 12), \
    ('PHID-REPO-two', '', 'runner', 'class', 'js', 2, 3), \
    ('PHID-REPO-two', '', 'run_all', 'class', 'js', 99, 0), \
    ('PHID-REPO-three', '', 'run', 'class', 'js', 2, 1); \
    INSERT INTO `{ns}_repository`.repository_commit (id, repositoryID, phid, \
      commitIdentifier, epoch, authorPHID, auditStatus, summary, importStatus) VALUES \
    (10, 1, 'PHID-CMIT-a', 'aaaaa11111aaaaa11111aaaaa11111aaaaa11111', 1, NULL, 'none', \
      'A', 15), \
    (11, 1, 'PHID-CMIT-b', 'aaaaa22222aaaaa22222aaaaa22222aaaaa22222', 1, NULL, 'none', \
      'B', 15); \
    INSERT INTO `{ns}_repository`.repository_branch (id, repositoryID, name, lintCommit, \
      dateCreated, dateModified) VALUES (5, 1, 'main', NULL, 1, 1); \
    INSERT INTO `{ns}_repository`.repository_coverage (branchID, pathID, commitID, \
      coverage) VALUES (5, 2, 10, 'UUU')";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "repository",
        "user",
        "policy",
        "spaces",
        "conduit",
        "auth",
        "project",
    ])
    .await?;
    test_db.execute(&SEED.replace("TOKEN", TOKEN)).await;
    let mut config = Config::default();
    let mut values = serde_json::Map::new();
    values.insert(
        "phabricator.base-uri".to_owned(),
        json!("https://phabricator.test/"),
    );
    config.push_layer("test", values);
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn call(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_diffusion::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, method, &http, Mode::Serve, NOW)
        .await
        .response
}

fn result(response: Response) -> Value {
    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = response
    else {
        panic!("expected an answer, got {response:?}");
    };
    result
}

fn core_error(message: &str) -> Response {
    Response::Envelope(Envelope::error("ERR-CONDUIT-CORE", message))
}

async fn rows(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("rows")
        .iter()
        .map(|row| phab_db::rows::text(row, "value").expect("value"))
        .collect()
}

#[tokio::test]
async fn finds_symbols() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let find = |params: Value| call(&context, "diffusion.findsymbols", params);

    // Repository 3's symbol is hidden; a missing path is null.
    assert_eq!(
        result(find(json!({"namePrefix": "run"})).await),
        json!([
            {
                "name": "run",
                "context": "Foo",
                "type": "function",
                "language": "php",
                "path": "/src/a b.c",
                "line": "12",
                "uri": "https://phabricator.test/diffusion/ONE/browse/main%252Fx/src/a%20b.c$12",
                "repositoryPHID": "PHID-REPO-one",
            },
            {
                "name": "run_all",
                "context": "",
                "type": "class",
                "language": "js",
                "path": null,
                "line": "0",
                "uri": "https://phabricator.test/source/two/browse/master/$0",
                "repositoryPHID": "PHID-REPO-two",
            },
            {
                "name": "runner",
                "context": "",
                "type": "class",
                "language": "js",
                "path": "/README",
                "line": "3",
                "uri": "https://phabricator.test/source/two/browse/master/README$3",
                "repositoryPHID": "PHID-REPO-two",
            },
        ])
    );
    let names = |value: Value| {
        value
            .as_array()
            .expect("a list")
            .iter()
            .map(|symbol| symbol["name"].as_str().expect("a name").to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(result(find(json!({"name": "run"})).await)), ["run"]);
    // "_" is not a wildcard.
    assert_eq!(
        names(result(find(json!({"namePrefix": "run_"})).await)),
        ["run_all"]
    );
    // An empty context is a condition; empty languages and types are not.
    assert_eq!(
        names(result(
            find(json!({"namePrefix": "run", "context": "", "language": "", "type": "0"})).await
        )),
        ["run_all", "runner"]
    );
    assert_eq!(
        names(result(
            find(json!({"namePrefix": "r", "language": "js", "type": "class"})).await
        )),
        ["run_all", "runner"]
    );
    assert_eq!(
        names(result(
            find(json!({"namePrefix": "r", "repositoryPHID": "PHID-REPO-one"})).await
        )),
        ["run"]
    );
    assert_eq!(result(find(json!({"name": "nothing"})).await), json!([]));

    assert_eq!(
        find(json!({"name": "run", "namePrefix": "r"})).await,
        core_error("You can not set both a name and a name prefix!")
    );
    assert_eq!(
        find(json!({"name": "0"})).await,
        core_error("You must set a name or a name prefix!")
    );
    assert!(matches!(
        find(json!({"name": 5})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn updates_coverage() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let update = |params: Value| call(&context, "diffusion.updatecoverage", params);
    let coverage = || async {
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', branchID, pathID, commitID, CAST(coverage AS CHAR)) AS value \
             FROM `{ns}_repository`.repository_coverage ORDER BY branchID, pathID, commitID",
        )
        .await
    };

    // A new branch; paths are normalized.
    let params = json!({
        "repositoryPHID": "PHID-REPO-one",
        "branch": "dev",
        "commit": "aaaaa1",
        "coverage": {"//src/a b.c/": "NCU", "README": "C"},
    });
    assert_eq!(result(update(params).await), Value::Null);
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', id, name, dateCreated) AS value \
             FROM `{ns}_repository`.repository_branch ORDER BY id",
        )
        .await,
        ["5 main 1", &format!("6 dev {NOW}")]
    );
    assert_eq!(coverage().await, ["5 2 10 UUU", "6 1 10 NCU", "6 2 10 C"]);

    // Update keeps the other paths and commits; overwrite drops them.
    let params = json!({
        "repositoryPHID": "PHID-REPO-one",
        "branch": "main",
        "commit": "aaaaa22222aaaaa22222aaaaa22222aaaaa22222",
        "coverage": {"README": "N"},
        "mode": "update",
    });
    assert_eq!(result(update(params).await), Value::Null);
    assert_eq!(
        coverage().await,
        ["5 2 10 UUU", "5 2 11 N", "6 1 10 NCU", "6 2 10 C"]
    );
    let params = json!({
        "repositoryPHID": "PHID-REPO-one",
        "branch": "main",
        "commit": "aaaaa1",
        "coverage": {"README": "X"},
        "mode": "",
    });
    assert_eq!(result(update(params).await), Value::Null);
    assert_eq!(coverage().await, ["5 2 10 X", "6 1 10 NCU", "6 2 10 C"]);

    // Errors.
    let call_with = |changes: Value| {
        let mut params = json!({
            "repositoryPHID": "PHID-REPO-one",
            "branch": "main",
            "commit": "aaaaa1",
            "coverage": {"README": "C"},
        });
        for (key, value) in changes.as_object().expect("changes") {
            params[key] = value.clone();
        }
        update(params)
    };
    assert_eq!(
        call_with(json!({"repositoryPHID": "PHID-REPO-nope"})).await,
        core_error("No repository exists with PHID \"PHID-REPO-nope\".")
    );
    for commit in ["aaaaa", "aaaa", "bbbbbbb"] {
        let expected = if commit == "aaaaa" {
            core_error("Expected a single result!")
        } else {
            core_error(&format!("No commit exists with identifier \"{commit}\"."))
        };
        assert_eq!(
            call_with(json!({"commit": commit})).await,
            expected,
            "{commit}"
        );
    }
    // The branch is created before the mode is checked.
    assert_eq!(
        call_with(json!({"branch": "late", "mode": "replace"})).await,
        core_error("Invalid mode \"replace\".")
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT name AS value FROM `{ns}_repository`.repository_branch WHERE id = 7",
        )
        .await,
        ["late"]
    );

    // Left to PHP, before anything is written.
    for changes in [
        json!({"repositoryPHID": "PHID-REPO-three"}),
        json!({"commit": "rONEaaaaa1"}),
        json!({"coverage": {"nope": "C"}, "branch": "unwritten"}),
        json!({"coverage": {}}),
        json!({"coverage": ["C"]}),
        json!({"coverage": {"README": 1}}),
        json!({"branch": 5}),
        json!({"mode": 1}),
        json!({"branch": "b".repeat(129)}),
    ] {
        let response = call_with(changes.clone()).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{changes}: {response:?}"
        );
    }
    assert_eq!(
        rows(
            &test_db,
            "SELECT CAST(COUNT(*) AS CHAR) AS value FROM `{ns}_repository`.repository_branch",
        )
        .await,
        ["3"]
    );

    test_db.drop_databases().await;
}
