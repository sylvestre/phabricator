//! `differential.revision.edit` referencing files, against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-fafafafafafafafafafafafafafa";

async fn edit(context: &Context, text: &str) -> Response {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
    phab_app_files::register(&mut registry);
    let params = json!({
        "objectIdentifier": "D1",
        "transactions": [{"type": "comment", "value": text}],
        "__conduit__": {"token": TOKEN},
    });
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(
        context,
        &registry,
        "differential.revision.edit",
        &http,
        Mode::Serve,
        1_700_000_000,
    )
    .await
    .response
}

async fn rows(test_db: &TestDatabase, sql: &str, column: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("rows")
        .iter()
        .map(|row| phab_db::rows::text(row, column).expect("column"))
        .collect()
}

async fn setup() -> Option<(TestDatabase, Context)> {
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
        "herald",
        "worker",
        "harbormaster",
    ])
    .await?;
    // Files 1 and 6 are visible to users, file 3 to no one.
    for (id, view) in [(1, "users"), (3, "no-one"), (6, "users")] {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_file`.file (id, phid, name, mimeType, byteSize, \
                   storageEngine, storageFormat, storageHandle, dateCreated, dateModified, \
                   authorPHID, secretKey, metadata, mailKey, viewPolicy, isExplicitUpload, \
                   isPartial, isDeleted) VALUES \
                 ({id}, 'PHID-FILE-{id}', 'f{id}.png', 'image/png', 1500, 'blob', 'raw', \
                   'h{id}', 1, 1, NULL, 'key{id:0>17}', '{{}}', 'm', '{view}', 1, 0, 0)"
            ))
            .await;
    }
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-bob', 1, 0)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn references_the_files_comments_name() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // F3 is hidden and F6 quoted: only F1 is referenced.
    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(&context, "Look: {F1}, F3 and F1 again.\n\n> {F6}").await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT(objectPHID, ' ', filePHID, ' ', attachmentMode, ' ', attacherPHID) \
             AS value FROM `{ns}_file`.file_attachment",
            "value"
        )
        .await,
        ["PHID-DREV-1 PHID-FILE-1 reference PHID-USER-bob"]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT(transactionType, ' ', oldValue, ' ', newValue) AS value FROM \
             `{ns}_differential`.differential_transaction ORDER BY id",
            "value"
        )
        .await,
        [
            "core:file [] {\"PHID-FILE-1\":\"reference\"}",
            "core:comment null null"
        ]
    );
    // A file the revision has already is left to PHP.
    assert!(matches!(
        edit(&context, "{F1} once more").await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
