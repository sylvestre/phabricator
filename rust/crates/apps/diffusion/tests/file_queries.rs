//! `diffusion.filecontentquery`, `diffusion.rawdiffquery` (which store
//! their output as files), `diffusion.diffquery` and `diffusion.looksoon`
//! against a test database and the working copy
//! `scripts/seed-dev-git-repo.sh` builds.

// One fixture, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use std::path::Path;

use phab_app_files::mime::MimeCommand;
use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-fqfqfqfqfqfqfqfqfqfqfqfqfqfq";
const NOW: i64 = 1_700_000_000;
/// The `file.integrity` key, in hex.
const KEY: &str = "00112233445566778899aabbccddeeff";

const C1: &str = "d61f24fe3e14bb3307532a9bc02968d7462cb336";
const C2: &str = "85948bd7ad4beca2df6fabad0d7f08b90365eff5";

/// The command PHP's answers come from, if this machine has it.
fn mime_command() -> Option<MimeCommand> {
    Path::new("/usr/bin/file")
        .exists()
        .then(|| MimeCommand::parse("/usr/bin/file --brief --mime"))
        .flatten()
}

async fn call_method(context: &Context, mode: Mode, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_diffusion::register_with(&mut registry, mime_command());
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, method, &http, mode, NOW)
        .await
        .response
}

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match call_method(context, Mode::Serve, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn rows(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("rows")
        .iter()
        .map(|row| {
            phab_db::rows::text(row, "value")
                .or_else(|_| phab_db::rows::int(row, "value").map(|value| value.to_string()))
                .expect("value")
        })
        .collect()
}

fn seed_repository(directory: &Path) {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/seed-dev-git-repo.sh");
    let output = std::process::Command::new("sh")
        .arg(script)
        .arg(directory)
        .output()
        .expect("running seed-dev-git-repo.sh");
    assert!(
        output.status.success(),
        "seed-dev-git-repo.sh: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn integrity(data: &[u8]) -> String {
    let key = hex::decode(KEY).unwrap();
    let data_hash = phab_core::hash::hmac_sha256(data, &key).unwrap();
    phab_core::hash::hmac_sha256(format!("{data_hash}/").as_bytes(), &key).unwrap()
}

async fn setup(directory: &Path) -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "repository",
        "project",
        "user",
        "policy",
        "spaces",
        "conduit",
        "auth",
        "cache",
        "file",
        "worker",
    ])
    .await?;
    let copy = directory.join("2");
    seed_repository(&copy);
    let copy = format!("{}/", copy.display());
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, \
               dateModified) VALUES ('file.integrity', '{KEY}', 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               repositorySlug, versionControlSystem, details, dateCreated, dateModified, \
               viewPolicy, editPolicy, pushPolicy, localPath, almanacServicePHID) VALUES \
             (1, 'PHID-REPO-git', 'Git', 'GIT', 'git-repo', 'git', '{{}}', 1, 1, 'users', \
               'admin', 'admin', '{copy}', NULL), \
             (3, 'PHID-REPO-secret', 'Secret', 'SEC', NULL, 'git', '{{}}', 1, 1, 'admin', \
               'admin', 'admin', '/nonexistent/', NULL); \
             INSERT INTO `{{ns}}_repository`.repository_statusmessage (repositoryID, \
               statusType, statusCode, parameters, epoch, messageCount) VALUES \
             (1, 'needs-update', 'okay', '[]', 5, 2)"
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phabricator.test/",
        "security.hmac-key": "hmac",
        "storage.mysql-engine.max-size": 8_388_608,
    }) {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

/// The file row and its attachment, without the random columns.
async fn stored(test_db: &TestDatabase, phid: &str) -> (String, String, String) {
    let row = rows(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' | ', IFNULL(name, 'NULL'), IFNULL(mimeType, 'NULL'), byteSize, \
               storageEngine, IFNULL(authorPHID, 'NULL'), metadata, IFNULL(ttl, 'NULL'), \
               isExplicitUpload, viewPolicy) AS value \
             FROM `{{ns}}_file`.file WHERE phid = '{phid}'"
        ),
    )
    .await
    .pop()
    .expect("file row");
    let data = rows(
        test_db,
        &format!(
            "SELECT CAST(b.data AS CHAR) AS value FROM `{{ns}}_file`.file f \
             JOIN `{{ns}}_file`.file_storageblob b ON b.id = f.storageHandle \
             WHERE f.phid = '{phid}'"
        ),
    )
    .await
    .pop()
    .expect("blob");
    let attachment = rows(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' | ', objectPHID, attachmentMode, IFNULL(attacherPHID, 'NULL'), \
               dateCreated) AS value FROM `{{ns}}_file`.file_attachment \
             WHERE filePHID = '{phid}'"
        ),
    )
    .await
    .pop()
    .expect("attachment");
    (row, data, attachment)
}

#[tokio::test]
async fn stores_file_contents_and_diffs() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let Some((test_db, context)) = setup(directory.path()).await else {
        return;
    };
    if mime_command().is_none() {
        test_db.drop_databases().await;
        return;
    }

    let result = answer(
        &context,
        "diffusion.filecontentquery",
        json!({"repository": "GIT", "path": "dir/a.txt", "commit": C2}),
    )
    .await;
    assert_eq!(result["tooSlow"], json!(false));
    assert_eq!(result["tooHuge"], json!(false));
    let phid = result["filePHID"].as_str().expect("a file").to_owned();
    let blob = b"one\ntwo\nthree\n";
    assert_eq!(
        stored(&test_db, &phid).await,
        (
            format!(
                "a.txt | text/plain | 14 | blob | NULL | \
                 {{\"storage\":[],\"integrity\":\"{}\"}} | {} | 0 | no-one",
                integrity(blob),
                NOW + 48 * 3600
            ),
            "one\ntwo\nthree\n".to_owned(),
            format!("PHID-REPO-git | attach | NULL | {NOW}"),
        )
    );
    // The file is queued for indexing.
    let tasks = rows(
        &test_db,
        "SELECT taskClass AS value FROM `{ns}_worker`.worker_activetask",
    )
    .await;
    assert_eq!(tasks, ["PhabricatorSearchWorker"]);

    // Over the byte limit, nothing is stored.
    let result = answer(
        &context,
        "diffusion.filecontentquery",
        json!({"repository": "GIT", "path": "README", "commit": C2, "byteLimit": 5}),
    )
    .await;
    assert_eq!(
        result,
        json!({"tooSlow": false, "tooHuge": true, "filePHID": null})
    );
    let files = rows(&test_db, "SELECT COUNT(*) AS value FROM `{ns}_file`.file").await;
    assert_eq!(files, ["1"]);

    // The raw diff of a commit, against its parent.
    let result = answer(
        &context,
        "diffusion.rawdiffquery",
        json!({"repository": "GIT", "commit": C2, "linesOfContext": 1}),
    )
    .await;
    let phid = result["filePHID"].as_str().expect("a file").to_owned();
    let (row, data, _) = stored(&test_db, &phid).await;
    assert!(row.starts_with(" | text/x-diff | "), "{row}");
    assert!(data.starts_with("diff --git a/README b/README\n"), "{data}");
    assert!(data.contains("@@ -1 +1,2 @@\n Hello\n+World\n"), "{data}");

    // Against another commit, at a path.
    let result = answer(
        &context,
        "diffusion.rawdiffquery",
        json!({"repository": "GIT", "commit": C2, "againstCommit": C1, "path": "README"}),
    )
    .await;
    let phid = result["filePHID"].as_str().expect("a file").to_owned();
    let (row, data, _) = stored(&test_db, &phid).await;
    assert!(row.starts_with("README | "), "{row}");
    assert_eq!(
        data,
        "diff --git a/README b/README\nindex e965047..f9264f7 100644\n--- a/README\n+++ \
         b/README\n@@ -1 +1,2 @@\n Hello\n+World\n"
    );

    // Shadow calls don't write.
    let response = call_method(
        &context,
        Mode::Shadow,
        "diffusion.filecontentquery",
        json!({"repository": "GIT", "path": "README", "commit": C2}),
    )
    .await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");
    // Git failures are left to PHP.
    let response = call_method(
        &context,
        Mode::Serve,
        "diffusion.filecontentquery",
        json!({"repository": "GIT", "path": "missing", "commit": C2}),
    )
    .await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");

    test_db.drop_databases().await;
}

#[tokio::test]
async fn parses_diffs_and_schedules_updates() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let Some((test_db, context)) = setup(directory.path()).await else {
        return;
    };

    let result = answer(
        &context,
        "diffusion.diffquery",
        json!({"repository": "GIT", "path": "dir/a.txt", "commit": C2}),
    )
    .await;
    assert_eq!(
        result,
        json!({
            "changes": {
                "dir/a.txt": {
                    "metadata": [],
                    "oldPath": null,
                    "currentPath": "dir/a.txt",
                    "awayPaths": [],
                    "oldProperties": [],
                    "newProperties": {"unix:filemode": "100644"},
                    "type": 1,
                    "fileType": 1,
                    "commitHash": null,
                    "hunks": [{
                        "oldOffset": "0",
                        "newOffset": "1",
                        "oldLength": "0",
                        "newLength": "3",
                        "addLines": 3,
                        "delLines": 0,
                        "isMissingOldNewline": false,
                        "isMissingNewNewline": false,
                        "corpus": "+one\n+two\n+three\n",
                    }],
                },
            },
            "effectiveCommit": C2,
        })
    );

    // diffusion.looksoon: once per repository the viewer can see.
    let result = answer(
        &context,
        "diffusion.looksoon",
        json!({"repositories": ["GIT", "rGIT", "git-repo", "SEC", "NOPE"], "urgency": "high"}),
    )
    .await;
    assert_eq!(result, Value::Null);
    let messages = rows(
        &test_db,
        "SELECT CONCAT_WS(' | ', repositoryID, statusType, statusCode, parameters, epoch, \
           messageCount) AS value FROM `{ns}_repository`.repository_statusmessage \
         ORDER BY repositoryID",
    )
    .await;
    assert_eq!(
        messages,
        [format!("1 | needs-update | okay | [] | {NOW} | 3")]
    );
    // The deprecated callsigns, when there are no repositories.
    answer(
        &context,
        "diffusion.looksoon",
        json!({"repositories": [], "callsigns": ["GIT"]}),
    )
    .await;
    let messages = rows(
        &test_db,
        "SELECT messageCount AS value FROM `{ns}_repository`.repository_statusmessage",
    )
    .await;
    assert_eq!(messages, ["4"]);
    assert_eq!(
        answer(&context, "diffusion.looksoon", json!({})).await,
        Value::Null
    );

    test_db.drop_databases().await;
}
