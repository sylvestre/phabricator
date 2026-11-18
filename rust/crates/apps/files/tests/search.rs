//! `file.search` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-ffffffffffffffffffffffffffff";

async fn query(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_files::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
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
        "file.search",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, params: Value) -> Value {
    match query(context, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{params}: expected an answer, got {other:?}"),
    }
}

async fn ids(context: &Context, params: Value) -> Vec<i64> {
    answer(context, params).await["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|file| file["id"].as_i64().expect("id"))
        .collect()
}

/// Files `(id, name, author, view policy, explicit, deleted)`.
const FILES: &[(i64, &str, &str, &str, i64, i64)] = &[
    // Bob's own file.
    (1, "mine.txt", "'PHID-USER-bob'", "no-one", 1, 0),
    // Attached to a revision Bob can see.
    (2, "shared.png", "NULL", "no-one", 1, 0),
    // Attached to a revision Bob can't see.
    (3, "secret.png", "NULL", "no-one", 1, 0),
    // A thumbnail of file 2.
    (4, "thumb.png", "NULL", "no-one", 0, 0),
    // Deleted.
    (5, "gone.txt", "'PHID-USER-bob'", "users", 1, 1),
    // Public to users.
    (6, "open.txt", "'PHID-USER-alice'", "users", 0, 0),
];

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
    ])
    .await?;
    for (id, name, author, view, explicit, deleted) in FILES {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_file`.file (id, phid, name, mimeType, byteSize, \
                   storageEngine, storageFormat, storageHandle, dateCreated, dateModified, \
                   authorPHID, secretKey, metadata, mailKey, viewPolicy, isExplicitUpload, \
                   isPartial, isDeleted) VALUES \
                 ({id}, 'PHID-FILE-{id}', '{name}', 'text/plain', 1500, 'blob', 'raw', \
                   'h{id}', {id}00, {id}01, {author}, 'key{id:0>17}', '{{}}', 'm', '{view}', \
                   {explicit}, 0, {deleted})"
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
             (1, 'Open', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', ''), \
             (2, 'Hidden', 'PHID-DREV-hidden', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'admin', 'admin', NULL, '{{}}', ''); \
             INSERT INTO `{{ns}}_file`.file_attachment (objectPHID, filePHID, attachmentMode, \
               dateCreated, dateModified) VALUES \
             ('PHID-DREV-open', 'PHID-FILE-2', 'attach', 1, 1), \
             ('PHID-DREV-hidden', 'PHID-FILE-3', 'attach', 1, 1); \
             INSERT INTO `{{ns}}_file`.file_transformedfile (originalPHID, transform, \
               transformedPHID, dateCreated, dateModified) VALUES \
             ('PHID-FILE-2', 'thumb-280x210', 'PHID-FILE-4', 1, 1)"
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
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
async fn shows_what_the_viewer_can_see() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Own, attached to a visible revision, its thumbnail, and open; not
    // the hidden revision's nor the deleted one.
    assert_eq!(ids(&context, json!({})).await, [6, 4, 2, 1]);
    assert_eq!(
        answer(&context, json!({"constraints": {"ids": [1]}})).await["data"][0],
        json!({
            "id": 1,
            "type": "FILE",
            "phid": "PHID-FILE-1",
            "fields": {
                "name": "mine.txt",
                "uri": "http://phabricator.test/F1",
                "dataURI": "http://phabricator.test/file/data/key00000000000000001/PHID-FILE-1/mine.txt",
                "size": 1500,
                "alt": {"custom": null, "default": "mine.txt (1 KB)"},
                "dateCreated": 100,
                "dateModified": 101,
                "policy": {"view": "no-one", "edit": "no-one"},
            },
            "attachments": {},
        })
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn filters_and_pages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        ids(&context, json!({"constraints": {"explicit": false}})).await,
        [6, 4]
    );
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"authorPHIDs": ["PHID-USER-bob"]}})
        )
        .await,
        [1]
    );
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"createdStart": 200, "createdEnd": 400}})
        )
        .await,
        [4, 2]
    );
    assert_eq!(
        ids(&context, json!({"order": "oldest", "limit": 2})).await,
        [1, 2]
    );
    assert_eq!(
        ids(&context, json!({"limit": 2, "after": "4"})).await,
        [2, 1]
    );
    // Files attached to other objects, and name searches, go to PHP.
    test_db
        .execute(
            "INSERT INTO `{ns}_file`.file_attachment (objectPHID, filePHID, attachmentMode, \
               dateCreated, dateModified) VALUES ('PHID-TASK-1', 'PHID-FILE-6', 'attach', 1, 1)",
        )
        .await;
    for params in [json!({}), json!({"constraints": {"name": "mine"}})] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn describes_one_file() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let mut registry = Registry::default();
    phab_app_files::register(&mut registry);
    let info = |params: Value| {
        let mut params = params;
        params["__conduit__"] = json!({"token": TOKEN});
        let body: String = form_urlencoded::Serializer::new(String::new())
            .append_pair("params", &params.to_string())
            .finish();
        HttpRequest {
            body,
            ..HttpRequest::default()
        }
    };
    let call = |http: HttpRequest| {
        let (context, registry) = (&context, &registry);
        async move {
            call::dispatch(
                context,
                registry,
                "file.info",
                &http,
                Mode::Shadow,
                1_700_000_000,
            )
            .await
            .response
        }
    };

    // Integers are MySQL's strings; deleted files are found too.
    let Response::Envelope(envelope) = call(info(json!({"id": 5}))).await else {
        panic!("expected an answer");
    };
    assert_eq!(
        envelope.result,
        json!({
            "id": "5",
            "phid": "PHID-FILE-5",
            "objectName": "F5",
            "name": "gone.txt",
            "mimeType": "text/plain",
            "byteSize": "1500",
            "authorPHID": "PHID-USER-bob",
            "dateCreated": "500",
            "dateModified": "501",
            "uri": "http://phabricator.test/F5",
        })
    );
    let Response::Envelope(envelope) = call(info(json!({"phid": "PHID-FILE-99"}))).await else {
        panic!("expected an error");
    };
    assert_eq!(envelope.error_code.as_deref(), Some("ERR-NOT-FOUND"));
    // PHP raises a policy exception for the hidden revision's file.
    assert!(matches!(
        call(info(json!({"phid": "PHID-FILE-3"}))).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
