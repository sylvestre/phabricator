//! `differential.creatediff`, `differential.createrawdiff` and
//! `differential.setdiffproperty` against a test database.

// One database, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-cdcdcdcdcdcdcdcdcdcdcdcdcdcd";
const NOW: i64 = 1_700_000_000;
/// A file's content hash, which fills its `binary(64)` column.
const CONTENT_HASH: &str = "ad5c1c1f0e8b0e2c6d0a3b3f9a1c8e7d6b5a49382716f5e4d3c2b1a0f9e8d7c6";

const DATABASES: &[&str] = &[
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "herald",
    "worker",
    "file",
];

async fn call(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
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

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn error(context: &Context, method: &str, params: Value) -> (String, String) {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info: Some(info),
            ..
        }) => (code, info),
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

async fn fallback(context: &Context, method: &str, params: Value) -> &'static str {
    match call(context, method, params.clone()).await {
        Response::Fallback(reason) => reason,
        other @ Response::Envelope(_) => {
            panic!("{method} {params}: expected PHP to answer, got {other:?}")
        }
    }
}

/// The rows of a query, as `CONCAT_WS()` lines aliased `line`.
async fn lines(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| phab_db::rows::text(row, "line").expect("a line"))
        .collect()
}

async fn count(test_db: &TestDatabase, table: &str) -> usize {
    lines(
        test_db,
        &format!("SELECT CAST(COUNT(*) AS CHAR) AS line FROM {table}"),
    )
    .await[0]
        .parse()
        .expect("a count")
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, uuid, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-open', 'Open', 'OPEN', 'uuid-open', 'git', '{{}}', 1, 1, 'users', \
               'admin', 'admin'), \
             (2, 'PHID-REPO-secret', 'Secret', 'SEC', NULL, 'git', '{{}}', 1, 1, 'admin', \
               'admin', 'admin'); \
             INSERT INTO `{{ns}}_file`.file (id, phid, name, mimeType, byteSize, storageEngine, \
               storageFormat, storageHandle, dateCreated, dateModified, authorPHID, secretKey, \
               metadata, mailKey, viewPolicy, isExplicitUpload, isPartial, isDeleted, \
               contentHash) VALUES \
             (1, 'PHID-FILE-image', 'a.png', 'image/png', 3, 'blob', 'raw', '1', 1, 1, \
               'PHID-USER-bob', 's', '{{}}', 'mmmmmmmmmmmmmmmmmmmm', 'users', 1, 0, 0, '{CONTENT_HASH}'); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Mine', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-bob', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-old'), \
             (2, 'Locked', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'PHID-USER-alice', NULL, '{{}}', 'PHID-DIFF-old2'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (100, 'PHID-DIFF-old', 1, 'PHID-USER-bob', 0, 0, 0, 1, 1, 'PHID-USER-bob'), \
             (101, 'PHID-DIFF-alice', NULL, 'PHID-USER-alice', 0, 0, 0, 1, 1, 'users'), \
             (102, 'PHID-DIFF-old2', 2, 'PHID-USER-alice', 0, 0, 0, 1, 1, 'users')"
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phabricator.test/",
        "phabricator.production-uri": "https://phabricator.example/",
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

/// A change as moz-phab sends it.
fn change(path: &str, corpus: &str) -> Value {
    json!({
        "metadata": {},
        "oldPath": path,
        "currentPath": path,
        "awayPaths": [],
        "oldProperties": {},
        "newProperties": {},
        "type": 2,
        "fileType": 1,
        "commitHash": "0123456789abcdef",
        "hunks": [{
            "oldOffset": 1,
            "oldLength": 2,
            "newOffset": 1,
            "newLength": 2,
            "addLines": 1,
            "delLines": 1,
            "isMissingOldNewline": false,
            "isMissingNewNewline": false,
            "corpus": corpus,
        }],
    })
}

fn create_params(changes: &Value) -> Value {
    json!({
        "changes": changes,
        "sourceMachine": "laptop",
        "sourcePath": "/home/bob/repo",
        "branch": "HEAD",
        "sourceControlSystem": "git",
        "sourceControlPath": "/",
        "sourceControlBaseRevision": "0123456789abcdef0123456789abcdef01234567",
        "creationMethod": "moz-phab-git",
        "lintStatus": "none",
        "unitStatus": "skip",
        "repositoryPHID": "PHID-REPO-open",
    })
}

#[tokio::test]
async fn creates_diffs_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // A moz-phab diff.
    let params = create_params(&json!([change("README", " a\n-b\n+c\n")]));
    let result = answer(&context, "differential.creatediff", params).await;
    let phid = result["phid"].as_str().expect("a PHID").to_owned();
    assert!(phid.starts_with("PHID-DIFF-"), "{phid}");
    assert_eq!(result["diffid"], json!(103));
    assert_eq!(
        result["uri"],
        json!("http://phabricator.test/differential/diff/103/")
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', id, revisionID, authorPHID, repositoryPHID, sourceMachine, \
               sourcePath, sourceControlSystem, sourceControlBaseRevision, sourceControlPath, \
               lintStatus, unitStatus, lineCount, branch, bookmark, creationMethod, \
               repositoryUUID, viewPolicy, dateCreated, dateModified) AS line \
             FROM `{ns}_differential`.differential_diff WHERE id = 103"
        )
        .await,
        [
            "103 PHID-USER-bob PHID-REPO-open laptop /home/bob/repo git \
          0123456789abcdef0123456789abcdef01234567 / 0 4 2 HEAD moz-phab-git uuid-open \
          PHID-USER-bob 1700000000 1700000000"
        ]
    );
    let hash = phab_core::hash::digest_for_index(b"a\nc\n");
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', diffID, oldFile, filename, awayPaths, changeType, fileType, \
               metadata, oldProperties, newProperties, addLines, delLines, \
               LEFT(phid, 10)) AS line FROM `{ns}_differential`.differential_changeset"
        )
        .await,
        [format!(
            "103 README README [] 2 1 {{\"line:first\":2,\"hash.effect\":\"{hash}\"}} [] [] 1 1 \
             PHID-DCNG-"
        )]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', oldOffset, oldLen, newOffset, newLen, dataType, \
               dataEncoding, dataFormat, CAST(data AS CHAR), dateCreated) AS line \
             FROM `{ns}_differential`.differential_hunk"
        )
        .await,
        ["1 2 1 2 text utf8 byte  a\n-b\n+c\n 1700000000"]
    );
    let transactions = lines(
        &test_db,
        "SELECT CONCAT_WS(' ', phid, authorPHID, objectPHID, viewPolicy, editPolicy, \
           transactionType, oldValue, newValue, contentSource, metadata) AS line \
         FROM `{ns}_differential`.differential_difftransaction",
    )
    .await;
    assert_eq!(transactions.len(), 1);
    let parts: Vec<&str> = transactions[0].split(' ').collect();
    assert!(parts[0].starts_with("PHID-XACT-DIFF-"), "{}", parts[0]);
    assert_eq!(
        parts[1..9],
        [
            "PHID-USER-bob",
            phid.as_str(),
            "public",
            "PHID-USER-bob",
            "differential:diff:create",
            "null",
            "true",
            r#"{"source":"conduit","params":[]}"#,
        ]
    );
    assert!(parts[9].starts_with(r#"{"core.groupID":""#), "{}", parts[9]);
    let task = lines(
        &test_db,
        "SELECT CONCAT_WS(' ', t.taskClass, t.priority, t.objectPHID, d.data) AS line \
         FROM `{ns}_worker`.worker_activetask t JOIN `{ns}_worker`.worker_taskdata d \
           ON d.id = t.dataID",
    )
    .await;
    let expected_state = r#""state":{"parentMessageID":null,"isNewObject":true,"heraldEmailPHIDs":[],"heraldForcedEmailPHIDs":[],"heraldHeader":null,"mailToPHIDs":[],"mailCCPHIDs":[],"feedNotifyPHIDs":[],"feedRelatedPHIDs":[],"feedShouldPublish":false,"mailShouldSend":false,"mustEncrypt":[],"mailStamps":[],"mailUnexpandablePHIDs":[],"mailMutedPHIDs":[],"webhookMap":[],"silent":null,"sendHistory":false,"excludeMailRecipientPHIDs":[],"custom":[],"custom.encoding":[]}}"#;
    assert_eq!(task.len(), 1);
    assert!(
        task[0].starts_with(&format!(
            "PhabricatorApplicationTransactionPublishWorker 1000 {phid} \
             {{\"objectPHID\":\"{phid}\",\"actorPHID\":\"PHID-USER-bob\",\
             \"xactionPHIDs\":[\"{}\"],",
            parts[0]
        )) && task[0].ends_with(expected_state),
        "{}",
        task[0]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', diffID, name, data, dateCreated) AS line \
             FROM `{ns}_differential`.differential_diffproperty"
        )
        .await,
        Vec::<String>::new()
    );

    // A large hunk is deflated as zlib does; a binary file whose new
    // version is a stored file hashes the file's content hash; a hidden
    // repository is a policy exception left to PHP.
    let corpus = (0..200).fold(String::new(), |corpus, line| {
        corpus + &format!("+line {line}\n")
    });
    let mut binary = change("a.png", "");
    binary["hunks"] = json!([]);
    binary["fileType"] = json!(2);
    binary["metadata"] = json!({"new:binary-phid": "PHID-FILE-image"});
    let mut params = create_params(&json!([change("big", &corpus), binary]));
    params["repositoryPHID"] = json!("PHID-REPO-unknown");
    params["sourceControlBaseRevision"] = json!(null);
    let result = answer(&context, "differential.creatediff", params).await;
    assert_eq!(result["diffid"], json!(104));
    let hunk = lines(
        &test_db,
        "SELECT CONCAT_WS(' ', dataFormat, HEX(data)) AS line \
         FROM `{ns}_differential`.differential_hunk WHERE id = 2",
    )
    .await;
    let deflated = phab_diff::deflate::gzdeflate(corpus.as_bytes());
    let hex: String = deflated
        .iter()
        .fold(String::new(), |hex, byte| hex + &format!("{byte:02X}"));
    assert_eq!(hunk, [format!("gzde {hex}")]);
    let file_hash =
        phab_core::hash::digest_for_index(format!("file-hash:{CONTENT_HASH}").as_bytes());
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', filename, metadata) AS line \
             FROM `{ns}_differential`.differential_changeset WHERE diffID = 104 ORDER BY id"
        )
        .await[1],
        format!(
            "a.png {{\"new:binary-phid\":\"PHID-FILE-image\",\"hash.effect\":\"{file_hash}\"}}"
        )
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', repositoryPHID, lineCount) AS line \
             FROM `{ns}_differential`.differential_diff WHERE id = 104"
        )
        .await,
        ["PHID-REPO-unknown 2"]
    );

    // Errors, in PHP's order.
    let empty = json!({"changes": [], "sourcePath": "/"});
    assert_eq!(
        error(&context, "differential.creatediff", empty).await,
        (
            "ERR-NO-CONTENT".to_owned(),
            "Diff may not be empty.".to_owned()
        )
    );
    let mut params = create_params(&json!([change("x", "+x\n")]));
    // This fork's method has no `properties` parameter.
    params["properties"] = json!({"local:commits": {}});
    assert_eq!(
        error(&context, "differential.creatediff", params).await,
        (
            "ERR-CONDUIT-CALL".to_owned(),
            "API Method \"differential.creatediff\" does not define these parameters: \
             'properties'."
                .to_owned()
        )
    );
    let mut params = create_params(&json!([change("x", "+x\n")]));
    params["sourcePath"] = json!("");
    assert_eq!(
        error(&context, "differential.creatediff", params).await,
        (
            "ERR-USAGE-NO-SOURCE-PATH".to_owned(),
            "You must specify a source path.".to_owned()
        )
    );

    // Left to PHP, before anything is written.
    let mut params = create_params(&json!([change("x", "+x\n")]));
    params["repositoryPHID"] = json!("PHID-REPO-secret");
    fallback(&context, "differential.creatediff", params).await;
    let mut params = create_params(&json!([change("x", "+x\n")]));
    params["sourceControlSystem"] = json!("hg");
    fallback(&context, "differential.creatediff", params).await;
    let mut params = create_params(&json!([change("x", "+x\n")]));
    params["repositoryPHID"] = json!(null);
    fallback(&context, "differential.creatediff", params).await;
    let mut params = create_params(&json!([{"hunks": []}]));
    params["sourcePath"] = json!("/");
    fallback(&context, "differential.creatediff", params).await;
    test_db
        .execute(
            "INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, \
               ruleType, phid, isDisabled, triggerObjectPHID) VALUES \
             (1, 'Block', 'PHID-USER-alice', 'differential.diff', 1, 38, 1, 1, 'every', \
               'global', 'PHID-HRUL-block', 0, NULL)",
        )
        .await;
    let params = create_params(&json!([change("x", "+x\n")]));
    assert_eq!(
        fallback(&context, "differential.creatediff", params).await,
        "Herald rules on diffs"
    );
    assert_eq!(
        count(&test_db, "`{ns}_differential`.differential_diff").await,
        5
    );
    assert_eq!(count(&test_db, "`{ns}_worker`.worker_activetask").await, 2);
}

#[tokio::test]
async fn creates_raw_diffs_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let diff = "diff --git a/README b/README\nindex 1..2 100644\n--- a/README\n+++ b/README\n\
                @@ -1,2 +1,2 @@\n a\n-b\n+c\n\
                diff --git a/new b/new\nnew file mode 100755\n--- /dev/null\n+++ b/new\n\
                @@ -0,0 +1 @@\n+x\n";
    let result = answer(
        &context,
        "differential.createrawdiff",
        json!({"diff": diff, "repositoryPHID": "PHID-REPO-open"}),
    )
    .await;
    assert_eq!(result["id"], json!(103));
    assert_eq!(
        result["uri"],
        json!("https://phabricator.example/differential/diff/103/")
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', repositoryPHID, IFNULL(repositoryUUID, '-'), \
               IFNULL(sourcePath, '-'), creationMethod, lintStatus, unitStatus, lineCount, \
               viewPolicy) AS line FROM `{ns}_differential`.differential_diff WHERE id = 103"
        )
        .await,
        ["PHID-REPO-open - - web 4 4 3 PHID-USER-bob"]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', IFNULL(oldFile, '-'), filename, changeType, newProperties, \
               metadata) AS line FROM `{ns}_differential`.differential_changeset ORDER BY id"
        )
        .await,
        [
            format!(
                "README README 2 [] {{\"line:first\":2,\"hash.effect\":\"{}\"}}",
                phab_core::hash::digest_for_index(b"a\nc\n")
            ),
            format!(
                "- new 1 {{\"unix:filemode\":\"100755\"}} {{\"line:first\":1,\
                 \"hash.effect\":\"{}\"}}",
                phab_core::hash::digest_for_index(b"x\n")
            ),
        ]
    );

    assert_eq!(
        error(
            &context,
            "differential.createrawdiff",
            json!({"diff": " \n"})
        )
        .await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Can't parse an empty diff!".to_owned()
        )
    );
    assert_eq!(
        error(
            &context,
            "differential.createrawdiff",
            json!({"diff": diff, "repositoryPHID": "PHID-REPO-nope"})
        )
        .await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "No such repository \"PHID-REPO-nope\"!".to_owned()
        )
    );
    for params in [
        json!({"diff": "Index: x\n===\n"}),
        json!({"diff": diff, "viewPolicy": "obj.differential.author"}),
        json!({"diff": diff, "viewPolicy": "PHID-PLCY-abcdefghijklmnopqrst"}),
        json!({"diff": "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n"}),
    ] {
        fallback(&context, "differential.createrawdiff", params).await;
    }
    assert_eq!(
        count(&test_db, "`{ns}_differential`.differential_diff").await,
        4
    );

    // A view policy is a second transaction, which the publish task lists.
    let result = answer(
        &context,
        "differential.createrawdiff",
        json!({"diff": diff, "viewPolicy": "users"}),
    )
    .await;
    assert_eq!(result["id"], json!(104));
    assert_eq!(
        lines(
            &test_db,
            "SELECT viewPolicy AS line FROM `{ns}_differential`.differential_diff WHERE id = 104"
        )
        .await,
        ["users"]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', x.transactionType, x.oldValue, x.newValue) AS line \
               FROM `{ns}_differential`.differential_difftransaction x \
               JOIN `{ns}_differential`.differential_diff d ON d.phid = x.objectPHID \
               WHERE d.id = 104 ORDER BY x.id"
        )
        .await,
        [
            "differential:diff:create null true",
            "core:view-policy null \"users\"",
        ]
    );
    let data = lines(
        &test_db,
        "SELECT data AS line FROM `{ns}_worker`.worker_taskdata ORDER BY id DESC LIMIT 1",
    )
    .await;
    let data: Value = serde_json::from_str(&data[0]).unwrap();
    assert_eq!(data["xactionPHIDs"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn sets_diff_properties_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let set = |diff_id: Value, name: &str, data: &str| json!({"diff_id": diff_id, "name": name, "data": data});
    let properties = || {
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', diffID, name, data, dateCreated, dateModified) AS line \
             FROM `{ns}_differential`.differential_diffproperty ORDER BY id",
        )
    };

    // A diff of the viewer's revision, then again.
    assert_eq!(
        answer(
            &context,
            "differential.setdiffproperty",
            set(json!(100), "arc:lint", r#"[{"code":"E1"}]"#)
        )
        .await,
        Value::Null
    );
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_diffproperty SET dateCreated = 5, \
             dateModified = 5",
        )
        .await;
    answer(
        &context,
        "differential.setdiffproperty",
        set(json!("100"), "arc:lint", r#"{"a/b": "é"}"#),
    )
    .await;
    assert_eq!(
        properties().await,
        ["100 arc:lint {\"a\\/b\":\"\\u00e9\"} 5 1700000000"]
    );

    let shared = &context;
    let code = move |params: Value| async move {
        error(shared, "differential.setdiffproperty", params)
            .await
            .0
    };
    assert_eq!(code(set(json!(100), "", "{}")).await, "ERR_NO_NAME");
    assert_eq!(
        code(set(json!(100), "x", "{nope")).await,
        "ERR_INVALID_DATA"
    );
    assert_eq!(code(set(json!(999), "x", "{}")).await, "ERR_NOT_FOUND");
    // Alice's diff without a revision, which bob can see.
    assert_eq!(code(set(json!(101), "x", "{}")).await, "ERR_PERMISSIONS");
    // A revision bob can see but not edit: a policy exception.
    fallback(
        &context,
        "differential.setdiffproperty",
        set(json!(102), "x", "{}"),
    )
    .await;
    fallback(
        &context,
        "differential.setdiffproperty",
        set(json!(100), "x", "5"),
    )
    .await;
    assert_eq!(properties().await.len(), 1);
}
