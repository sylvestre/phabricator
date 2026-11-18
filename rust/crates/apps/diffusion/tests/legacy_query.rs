//! `repository.query` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-rqrqrqrqrqrqrqrqrqrqrqrqrqrq";

const DATABASES: &[&str] = &[
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

/// Repositories `(id, name, callsign, VCS, details, view policy, uuid)`.
const REPOSITORIES: &[(i64, &str, &str, &str, &str, &str, &str)] = &[
    (
        1,
        "Hosted",
        "'HOST'",
        "hg",
        r#"{"hosting-enabled":true,"tracking-enabled":"active","description":""}"#,
        "users",
        "NULL",
    ),
    (
        2,
        "Mirror",
        "NULL",
        "git",
        r#"{"remote-uri":"https://example.com/m.git","staging-uri":"https://stage.example.com/m.git","encoding":"ISO-8859-1"}"#,
        "users",
        "'uuid-2'",
    ),
    (3, "Secret", "'SEC'", "hg", "{}", "admin", "NULL"),
    (
        4,
        "Odd",
        "NULL",
        "git",
        r#"{"remote-uri":"git@example.com:odd.git"}"#,
        "users",
        "NULL",
    ),
];

async fn query(context: &Context, params: Value) -> Response {
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
    call::dispatch(
        context,
        &registry,
        "repository.query",
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

async fn ids(context: &Context, params: Value) -> Vec<String> {
    answer(context, params)
        .await
        .as_array()
        .expect("a list")
        .iter()
        .map(|repository| repository["id"].as_str().expect("id").to_owned())
        .collect()
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1)"
        ))
        .await;
    for (id, name, callsign, vcs, details, view, uuid) in REPOSITORIES {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
                   versionControlSystem, details, dateCreated, dateModified, viewPolicy, \
                   editPolicy, pushPolicy, uuid) VALUES \
                 ({id}, 'PHID-REPO-{id}', '{name}', {callsign}, '{vcs}', '{details}', 1, 1, \
                   '{view}', 'admin', 'admin', {uuid})"
            ))
            .await;
    }
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
async fn describes_repositories() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Lisk's strings, the details as they are, and staging for Git only.
    assert_eq!(
        answer(&context, json!({"ids": [1, 2]})).await,
        json!([
            {
                "id": "2",
                "name": "Mirror",
                "phid": "PHID-REPO-2",
                "callsign": null,
                "monogram": "R2",
                "vcs": "git",
                "uri": "http://phabricator.test/diffusion/2/",
                "remoteURI": "https://example.com/m.git",
                "description": null,
                "isActive": false,
                "isHosted": false,
                "isImporting": false,
                "encoding": "ISO-8859-1",
                "staging": {
                    "supported": true,
                    "prefix": "phabricator",
                    "uri": "https://stage.example.com/m.git",
                },
            },
            {
                "id": "1",
                "name": "Hosted",
                "phid": "PHID-REPO-1",
                "callsign": "HOST",
                "monogram": "rHOST",
                "vcs": "hg",
                "uri": "http://phabricator.test/diffusion/HOST/",
                "remoteURI": "",
                "description": "",
                "isActive": true,
                "isHosted": true,
                "isImporting": false,
                "encoding": "UTF-8",
                "staging": {"supported": false, "prefix": "phabricator", "uri": null},
            },
        ])
    );
    // PhutilURI rewrites the Git-style URI.
    assert!(matches!(
        query(&context, json!({})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn filters_orders_and_pages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let all = json!([1, 2, 3]);
    // The secret repository is hidden; newest first by default.
    assert_eq!(ids(&context, json!({"ids": all})).await, ["2", "1"]);
    assert_eq!(
        ids(&context, json!({"ids": all, "order": "oldest"})).await,
        ["1", "2"]
    );
    assert_eq!(
        ids(&context, json!({"ids": all, "order": "name"})).await,
        ["1", "2"]
    );
    assert_eq!(
        ids(&context, json!({"ids": all, "limit": 1, "after": "2"})).await,
        ["1"]
    );
    assert_eq!(ids(&context, json!({"uuids": ["uuid-2"]})).await, ["2"]);
    assert_eq!(
        ids(&context, json!({"callsigns": ["HOST"], "vcsTypes": ["hg"]})).await,
        ["1"]
    );
    assert_eq!(
        ids(
            &context,
            json!({"vcsTypes": ["git"], "callsigns": ["HOST"]})
        )
        .await,
        Vec::<String>::new()
    );
    for params in [
        json!({"remoteURIs": ["https://example.com/m.git"]}),
        json!({"ids": all, "order": ["id"]}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
