//! The file write methods (`file.upload`, `file.allocate`,
//! `file.uploadchunk`, `file.querychunks`, `file.download`) against a test
//! database.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use base64::Engine as _;
use phab_app_files::mime::MimeCommand;
use phab_app_files::storage::CHUNK_SIZE;
use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-fafafafafafafafafafafafafafa";
const NOW: i64 = 1_700_000_000;
/// The `file.integrity` key, in hex.
const KEY: &str = "00112233445566778899aabbccddeeff";
/// Bob's account secret: 64 characters, as PHP makes them.
const SECRET: &str = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijkl";

fn base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// The command PHP's answers come from, if this machine has it.
fn mime_command() -> Option<MimeCommand> {
    std::path::Path::new("/usr/bin/file")
        .exists()
        .then(|| MimeCommand::parse("/usr/bin/file --brief --mime"))
        .flatten()
}

async fn call(context: &Context, mime: bool, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_files::register_with(&mut registry, if mime { mime_command() } else { None });
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
    match call(context, true, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn error(context: &Context, method: &str, params: Value) -> (String, String) {
    match call(context, true, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info,
            ..
        }) => (code, error_info.unwrap_or_default()),
        other => panic!("{method} {params}: expected an error, got {other:?}"),
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

async fn count(test_db: &TestDatabase, table: &str) -> usize {
    rows(test_db, &format!("SELECT COUNT(*) AS value FROM {table}"))
        .await
        .first()
        .and_then(|value| value.parse().ok())
        .expect("count")
}

/// The file row PHP would show, without its random columns.
async fn file_row(test_db: &TestDatabase, phid: &str) -> String {
    rows(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' | ', IFNULL(name, 'NULL'), IFNULL(mimeType, 'NULL'), byteSize, \
               storageEngine, storageFormat, IFNULL(authorPHID, 'NULL'), \
               TRIM(TRAILING '\\0' FROM contentHash), metadata, IFNULL(ttl, 'NULL'), \
               isExplicitUpload, viewPolicy, isPartial, dateCreated, \
               LENGTH(secretKey), LENGTH(mailKey)) AS value \
             FROM `{{ns}}_file`.file WHERE phid = '{phid}'"
        ),
    )
    .await
    .pop()
    .expect("file row")
}

async fn handle(test_db: &TestDatabase, phid: &str) -> String {
    rows(
        test_db,
        &format!("SELECT storageHandle AS value FROM `{{ns}}_file`.file WHERE phid = '{phid}'"),
    )
    .await
    .pop()
    .expect("handle")
}

fn integrity(data: &[u8]) -> String {
    let key = hex::decode(KEY).unwrap();
    let data_hash = phab_core::hash::hmac_sha256(data, &key).unwrap();
    phab_core::hash::hmac_sha256(format!("{data_hash}/").as_bytes(), &key).unwrap()
}

async fn setup(max_size: i64) -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "file",
        "user",
        "conduit",
        "differential",
        "repository",
        "project",
        "policy",
        "spaces",
        "auth",
        "worker",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, '{SECRET}', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, \
               dateModified) VALUES ('file.integrity', '{KEY}', 1, 1); \
             INSERT INTO `{{ns}}_file`.file_storageblob (id, data, dateCreated, dateModified) \
               VALUES (7, 'stored', 1, 1); \
             INSERT INTO `{{ns}}_file`.file (id, phid, name, mimeType, byteSize, \
               storageEngine, storageFormat, storageHandle, dateCreated, dateModified, \
               authorPHID, secretKey, contentHash, metadata, mailKey, viewPolicy, \
               isExplicitUpload, isPartial, isDeleted) VALUES \
             (1, 'PHID-FILE-old', 'old.txt', 'text/plain; charset=us-ascii', 6, 'blob', 'raw', \
               '7', 1, 1, 'PHID-USER-bob', 'key00000000000000001', SHA2('stored', 256), \
               '{{\"storage\":[],\"integrity\":\"{}\",\"width\":0}}', 'm', 'users', 1, 0, 0), \
             (2, 'PHID-FILE-hidden', 'hidden.txt', 'text/plain', 6, 'blob', 'raw', '7', 1, 1, \
               NULL, 'key00000000000000002', NULL, '{{}}', 'm', 'no-one', 1, 0, 0)",
            integrity(b"stored")
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phabricator.test/",
        "security.hmac-key": "hmac",
        "storage.mysql-engine.max-size": max_size,
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

#[tokio::test]
async fn uploads_into_mysql() {
    let Some((test_db, context)) = setup(8_388_608).await else {
        return;
    };
    let Some(_) = mime_command() else {
        test_db.drop_databases().await;
        return;
    };

    let phid = answer(
        &context,
        "file.upload",
        json!({"data_base64": base64(b"hello\n"), "name": "hello.txt", "canCDN": true}),
    )
    .await;
    let phid = phid.as_str().expect("a PHID").to_owned();
    assert!(phid.starts_with("PHID-FILE-"));
    let hash = phab_app_files::storage::content_hash(b"hello\n");
    assert_eq!(
        file_row(&test_db, &phid).await,
        format!(
            "hello.txt | text/plain; charset=us-ascii | 6 | blob | raw | PHID-USER-bob | {hash} | \
             {{\"storage\":[],\"integrity\":\"{}\",\"canCDN\":1}} | NULL | 1 | users | 0 | {NOW} | \
             20 | 20",
            integrity(b"hello\n")
        )
    );
    let blob = handle(&test_db, &phid).await;
    assert_eq!(
        rows(
            &test_db,
            &format!(
                "SELECT CONCAT(data, ' ', dateCreated) AS value FROM \
                 `{{ns}}_file`.file_storageblob WHERE id = {blob}"
            ),
        )
        .await,
        [format!("hello\n {NOW}")]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', t.taskClass, t.priority, t.objectPHID, d.data) AS value \
             FROM `{ns}_worker`.worker_activetask t JOIN `{ns}_worker`.worker_taskdata d \
             ON d.id = t.dataID",
        )
        .await,
        [format!(
            "PhabricatorSearchWorker 3500 {phid} {{\"documentPHID\":\"{phid}\",\"parameters\":[],\
             \"strict\":false}}"
        )]
    );

    // The same content again shares the blob; nameless, with the policy.
    let copy = answer(
        &context,
        "file.upload",
        json!({"data_base64": base64(b"hello\n"), "viewPolicy": "no-one"}),
    )
    .await;
    let copy = copy.as_str().expect("a PHID").to_owned();
    assert_eq!(
        file_row(&test_db, &copy).await,
        format!(
            "NULL | text/plain; charset=us-ascii | 6 | blob | raw | PHID-USER-bob | {hash} | \
             {{\"storage\":[]}} | NULL | 1 | no-one | 0 | {NOW} | 20 | 20"
        )
    );
    assert_eq!(handle(&test_db, &copy).await, blob);
    assert_eq!(count(&test_db, "`{ns}_file`.file_storageblob").await, 2);

    // Both download, as does the file stored before.
    for phid in [&phid, &copy] {
        assert_eq!(
            answer(&context, "file.download", json!({"phid": phid})).await,
            json!(base64(b"hello\n"))
        );
    }
    assert_eq!(
        answer(&context, "file.download", json!({"phid": "PHID-FILE-old"})).await,
        json!(base64(b"stored"))
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn refuses_uploads_like_php() {
    let Some((test_db, context)) = setup(8_388_608).await else {
        return;
    };

    let core = |info: &str| ("ERR-CONDUIT-CORE".to_owned(), info.to_owned());
    assert_eq!(
        error(&context, "file.upload", json!({"name": "x"})).await,
        core("Field \"data_base64\" must be non-empty.")
    );
    assert_eq!(
        error(&context, "file.upload", json!({"data_base64": "a-b"})).await,
        core("Unable to decode base64 data!")
    );
    // New data needs its type detected.
    assert_eq!(
        call(
            &context,
            false,
            "file.upload",
            json!({"data_base64": base64(b"new")})
        )
        .await,
        Response::Fallback("detecting MIME types without PHAB_MIME_COMMAND")
    );
    // The content of a stored file doesn't.
    let copy = match call(
        &context,
        false,
        "file.upload",
        json!({"data_base64": base64(b"stored")}),
    )
    .await
    {
        Response::Envelope(Envelope { result, .. }) => result,
        other @ Response::Fallback(_) => panic!("expected an answer, got {other:?}"),
    };
    // Its dimensions are copied only when truthy.
    assert_eq!(
        file_row(&test_db, copy.as_str().unwrap()).await,
        format!(
            "NULL | text/plain; charset=us-ascii | 6 | blob | raw | PHID-USER-bob | {} | \
             {{\"storage\":[]}} | NULL | 1 | users | 0 | {NOW} | 20 | 20",
            phab_app_files::storage::content_hash(b"stored")
        )
    );
    assert_eq!(
        call(
            &context,
            true,
            "file.upload",
            json!({"data_base64": "eA==", "name": 5})
        )
        .await,
        Response::Fallback("file names which are not strings")
    );
    assert_eq!(
        answer(
            &context,
            "file.uploadhash",
            json!({"hash": "x", "name": "y"})
        )
        .await,
        Value::Null
    );

    assert_eq!(
        error(&context, "file.download", json!({"phid": "PHID-FILE-none"})).await,
        ("ERR-BAD-PHID".to_owned(), "No such file exists.".to_owned())
    );
    assert_eq!(
        call(
            &context,
            true,
            "file.download",
            json!({"phid": "PHID-FILE-hidden"})
        )
        .await,
        Response::Fallback("a file the viewer can't see")
    );
    assert_eq!(
        error(
            &context,
            "file.querychunks",
            json!({"filePHID": "PHID-FILE-old"})
        )
        .await,
        core("File \"PHID-FILE-old\" does not have chunks!")
    );
    assert_eq!(
        error(
            &context,
            "file.querychunks",
            json!({"filePHID": "PHID-FILE-none"})
        )
        .await,
        core("No such file \"PHID-FILE-none\"!")
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn allocates_like_php() {
    let Some((test_db, context)) = setup(8_388_608).await else {
        return;
    };

    // Small files are uploaded directly.
    assert_eq!(
        answer(
            &context,
            "file.allocate",
            json!({"name": "a.txt", "contentLength": 10})
        )
        .await,
        json!({"upload": true, "filePHID": null})
    );
    // A known hash makes a copy, with the TTL.
    let hash = phab_app_files::storage::content_hash(b"stored");
    let found = answer(
        &context,
        "file.allocate",
        json!({"name": "again.txt", "contentLength": 6, "contentHash": hash,
               "deleteAfterEpoch": NOW + 60}),
    )
    .await;
    assert_eq!(found["upload"], json!(false));
    assert_eq!(
        file_row(&test_db, found["filePHID"].as_str().unwrap()).await,
        format!(
            "again.txt | text/plain; charset=us-ascii | 6 | blob | raw | PHID-USER-bob | {hash} | \
             {{\"storage\":[]}} | {} | 1 | users | 0 | {NOW} | 20 | 20",
            NOW + 60
        )
    );
    assert_eq!(
        error(
            &context,
            "file.allocate",
            json!({"name": "a", "contentLength": 6, "contentHash": hash,
                   "deleteAfterEpoch": NOW - 1}),
        )
        .await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            format!(
                "Absolute TTL must be in the present or future, but TTL \"{}\" is in the past.",
                NOW - 1
            )
        )
    );

    // A large file gets chunks; its hash is the user's chunked hash.
    let length = 9_000_000;
    let large = answer(
        &context,
        "file.allocate",
        json!({"name": "big.bin", "contentLength": length, "contentHash": "abc"}),
    )
    .await;
    assert_eq!(large["upload"], json!(true));
    let phid = large["filePHID"].as_str().unwrap().to_owned();
    let chunked =
        phab_core::hash::weak_digest(format!("{SECRET}:abc:PHID-USER-bob").as_bytes(), b"hmac")
            .unwrap();
    assert_eq!(
        file_row(&test_db, &phid).await,
        format!(
            "big.bin | application/octet-stream | {length} | chunks | raw | PHID-USER-bob | {}-C | \
             [] | NULL | 1 | users | 1 | {NOW} | 20 | 20",
            &chunked[..38]
        )
    );
    let chunk_handle = handle(&test_db, &phid).await;
    assert_eq!(chunk_handle.len(), 12);
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', byteStart, byteEnd, IFNULL(dataFilePHID, 'NULL')) AS value \
             FROM `{ns}_file`.file_chunk ORDER BY id",
        )
        .await,
        [
            "0 4194304 NULL",
            "4194304 8388608 NULL",
            "8388608 9000000 NULL"
        ]
    );
    assert_eq!(
        answer(
            &context,
            "file.querychunks",
            json!({"filePHID": phid.clone()})
        )
        .await,
        json!([
            {"byteStart": "8388608", "byteEnd": "9000000", "complete": false},
            {"byteStart": "4194304", "byteEnd": "8388608", "complete": false},
            {"byteStart": "0", "byteEnd": "4194304", "complete": false},
        ])
    );
    // Without a hash, the partial file is found by name and length.
    assert_eq!(
        answer(
            &context,
            "file.allocate",
            json!({"name": "big.bin", "contentLength": length.to_string()})
        )
        .await,
        json!({"upload": true, "filePHID": phid})
    );
    // With another name, it gets new chunks and a random hash.
    let other = answer(
        &context,
        "file.allocate",
        json!({"name": "other.bin", "contentLength": length}),
    )
    .await;
    assert_ne!(other["filePHID"], json!(phid));
    assert_eq!(count(&test_db, "`{ns}_file`.file_chunk").await, 6);
    assert_eq!(count(&test_db, "`{ns}_worker`.worker_activetask").await, 3);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn uploads_chunks() {
    let Some((test_db, context)) = setup(CHUNK_SIZE).await else {
        return;
    };
    let Some(_) = mime_command() else {
        test_db.drop_databases().await;
        return;
    };

    let length = CHUNK_SIZE + 10;
    let phid = answer(
        &context,
        "file.allocate",
        json!({"name": "big.txt", "contentLength": length}),
    )
    .await["filePHID"]
        .as_str()
        .expect("a chunked file")
        .to_owned();
    let id = rows(
        &test_db,
        &format!("SELECT id AS value FROM `{{ns}}_file`.file WHERE phid = '{phid}'"),
    )
    .await
    .pop()
    .unwrap();

    let core = |info: &str| ("ERR-CONDUIT-CORE".to_owned(), info.to_owned());
    assert_eq!(
        error(
            &context,
            "file.uploadchunk",
            json!({"filePHID": phid, "byteStart": 5, "data": "x"}),
        )
        .await,
        core("Chunk start byte is 0, not 5.")
    );
    assert_eq!(
        error(
            &context,
            "file.uploadchunk",
            json!({"filePHID": phid, "byteStart": 0, "data": "x", "dataEncoding": "hex"}),
        )
        .await,
        core("Unsupported data encoding.")
    );
    assert_eq!(
        error(
            &context,
            "file.uploadchunk",
            json!({"filePHID": phid, "byteStart": length, "data": "x"}),
        )
        .await,
        core(&format!(
            "There are no file data chunks in byte range {length} - {}.",
            length + 1
        ))
    );

    // The last chunk first, in base64: its type is given.
    let tail = b"0123456789";
    assert_eq!(
        answer(
            &context,
            "file.uploadchunk",
            json!({"filePHID": phid, "byteStart": CHUNK_SIZE.to_string(),
                   "data": base64(tail), "dataEncoding": "base64"}),
        )
        .await,
        Value::Null
    );
    let data_phid = rows(
        &test_db,
        &format!(
            "SELECT dataFilePHID AS value FROM `{{ns}}_file`.file_chunk \
             WHERE byteStart = {CHUNK_SIZE}"
        ),
    )
    .await
    .pop()
    .unwrap();
    let chunk_id = rows(
        &test_db,
        &format!("SELECT id AS value FROM `{{ns}}_file`.file_chunk WHERE byteStart = {CHUNK_SIZE}"),
    )
    .await
    .pop()
    .unwrap();
    assert_eq!(
        file_row(&test_db, &data_phid).await,
        format!(
            "F{id}.chunk-{chunk_id} | application/octet-stream | 10 | blob | raw | NULL | {} | \
             {{\"storage\":[],\"integrity\":\"{}\",\"chunk\":true}} | NULL | 0 | no-one | 0 | \
             {NOW} | 20 | 20",
            phab_app_files::storage::content_hash(tail),
            integrity(tail)
        )
    );
    assert_eq!(
        error(
            &context,
            "file.uploadchunk",
            json!({"filePHID": phid, "byteStart": CHUNK_SIZE,
                   "data": base64(tail), "dataEncoding": "base64"}),
        )
        .await,
        core("Chunk has already been uploaded.")
    );
    assert!(file_row(&test_db, &phid).await.contains(" | 1 | "));

    // The first chunk: its type is detected and becomes the file's, which
    // is complete.
    let head = "a".repeat(usize::try_from(CHUNK_SIZE).unwrap());
    assert_eq!(
        answer(
            &context,
            "file.uploadchunk",
            json!({"filePHID": phid, "byteStart": 0, "data": head}),
        )
        .await,
        Value::Null
    );
    assert_eq!(
        file_row(&test_db, &phid)
            .await
            .split(" | ")
            .collect::<Vec<_>>()[1..4],
        [
            "text/plain; charset=us-ascii",
            &length.to_string(),
            "chunks"
        ]
    );
    assert_eq!(
        rows(
            &test_db,
            &format!(
                "SELECT CONCAT_WS(' ', isPartial, dateModified) AS value FROM \
                 `{{ns}}_file`.file WHERE phid = '{phid}'"
            ),
        )
        .await,
        [format!("0 {NOW}")]
    );
    assert_eq!(
        answer(&context, "file.querychunks", json!({"filePHID": phid})).await,
        json!([
            {"byteStart": "4194304", "byteEnd": "4194314", "complete": true},
            {"byteStart": "0", "byteEnd": "4194304", "complete": true},
        ])
    );
    let mut data = head.into_bytes();
    data.extend(tail);
    assert_eq!(
        answer(&context, "file.download", json!({"phid": phid})).await,
        json!(base64(&data))
    );
    // Only the chunked file is indexed.
    assert_eq!(count(&test_db, "`{ns}_worker`.worker_activetask").await, 1);

    test_db.drop_databases().await;
}
