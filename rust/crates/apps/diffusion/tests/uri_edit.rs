//! `diffusion.uri.edit` against a test database.
//!
//! Alice, who calls, edits the URIs of repository 1: URI 1 observes
//! GitHub with a credential, URI 2 mirrors to GitHub, URI 3 is disabled
//! and URI 4 is a builtin URI the configuration disables. Repository 2 is
//! public, so its builtin HTTP URI (5) is in use; URI 7 is one of its own.
//! She can see repository 3 (URI 6) but not edit it.

// One fixture, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-urururururururururururururur";
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
      pushPolicy, localPath, credentialPHID) VALUES \
    (1, 'PHID-REPO-one', 'One', 'ONE', NULL, 'git', \
      '{\"tracking-enabled\":\"active\",\"remote-uri\":\"https:\\/\\/github.com\\/mozilla\\/one.git\",\
\"hosting-enabled\":false}', 1, 1, 'users', 'users', 'users', '/var/repo/1/', 'PHID-CDTL-one'), \
    (2, 'PHID-REPO-two', 'Two', NULL, 'two', 'git', '{}', 1, 1, 'public', 'users', \
      'users', '/var/repo/2/', NULL), \
    (3, 'PHID-REPO-three', 'Three', 'THREE', NULL, 'git', '{}', 1, 1, 'users', 'admin', \
      'users', '/var/repo/3/', NULL); \
    INSERT INTO `{ns}_repository`.repository_uri (id, phid, repositoryPHID, uri, \
      builtinProtocol, builtinIdentifier, ioType, displayType, isDisabled, dateCreated, \
      dateModified, credentialPHID) VALUES \
    (1, 'PHID-RURI-1', 'PHID-REPO-one', 'https://github.com/mozilla/one.git', NULL, NULL, \
      'observe', 'default', 0, 1, 1, 'PHID-CDTL-one'), \
    (2, 'PHID-RURI-2', 'PHID-REPO-one', 'git@github.com:mozilla/one-mirror.git', NULL, \
      NULL, 'mirror', 'default', 0, 1, 1, NULL), \
    (3, 'PHID-RURI-3', 'PHID-REPO-one', 'https://old.example.com/x', NULL, NULL, 'none', \
      'default', 1, 1, 1, NULL), \
    (4, 'PHID-RURI-4', 'PHID-REPO-one', 'ssh://callsign', 'ssh', 'callsign', 'default', \
      'default', 1, 1, 1, NULL), \
    (5, 'PHID-RURI-5', 'PHID-REPO-two', 'http://id', 'http', 'id', 'default', 'default', 1, \
      1, 1, NULL), \
    (6, 'PHID-RURI-6', 'PHID-REPO-three', 'https://example.com/three', NULL, NULL, 'none', \
      'default', 0, 1, 1, NULL), \
    (7, 'PHID-RURI-7', 'PHID-REPO-two', 'https://example.com/two', NULL, NULL, 'none', \
      'default', 0, 1, 1, NULL); \
    INSERT INTO `{ns}_repository`.repository_uriindex (repositoryPHID, repositoryURI) \
      VALUES ('PHID-REPO-one', 'stale'); \
    INSERT INTO `{ns}_repository`.repository_statusmessage (repositoryID, statusType, \
      statusCode, parameters, epoch, messageCount) VALUES \
    (1, 'needs-update', 'okay', '[]', 5, 1), (1, 'fetch', 'okay', '[]', 5, 1)";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "repository",
        "user",
        "policy",
        "spaces",
        "conduit",
        "auth",
        "worker",
        "project",
        "herald",
    ])
    .await?;
    test_db.execute(&SEED.replace("TOKEN", TOKEN)).await;
    let mut config = Config::default();
    let mut values = serde_json::Map::new();
    values.insert(
        "phabricator.base-uri".to_owned(),
        json!("https://phabricator.example.com/"),
    );
    config.push_layer("test", values);
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn call(context: &Context, params: Value) -> Response {
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
        "diffusion.uri.edit",
        &http,
        Mode::Serve,
        NOW,
    )
    .await
    .response
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

fn edit(id: Value, transactions: Value) -> Value {
    Value::Object(
        [
            ("objectIdentifier".to_owned(), id),
            ("transactions".to_owned(), transactions),
        ]
        .into_iter()
        .collect(),
    )
}

fn error(message: &str) -> Response {
    Response::Envelope(Envelope::error(
        "ERR-CONDUIT-CORE",
        format!("Validation errors:\n  - {message}"),
    ))
}

async fn succeeds(context: &Context, params: Value) -> Value {
    let response = call(context, params).await;
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

/// The repository's details and credential, and its URI index.
async fn repository(test_db: &TestDatabase) -> (Vec<String>, Vec<String>) {
    (
        rows(
            test_db,
            "SELECT CONCAT_WS(' | ', details, IFNULL(credentialPHID, 'NULL'), dateModified) \
               AS value FROM `{ns}_repository`.repository WHERE id = 1",
        )
        .await,
        rows(
            test_db,
            "SELECT repositoryURI AS value FROM `{ns}_repository`.repository_uriindex \
             WHERE repositoryPHID = 'PHID-REPO-one' ORDER BY id",
        )
        .await,
    )
}

#[tokio::test]
async fn edits_uris() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // A new path keeps the credential.
    let result = succeeds(
        &context,
        edit(
            json!("PHID-RURI-1"),
            json!([{"type": "uri", "value": "https://github.com/mozilla/one-new.git"}]),
        ),
    )
    .await;
    assert_eq!(result["object"], json!({"id": 1, "phid": "PHID-RURI-1"}));
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        repository(&test_db).await,
        (
            vec![format!(
                "{{\"tracking-enabled\":\"active\",\
                 \"remote-uri\":\"https:\\/\\/github.com\\/mozilla\\/one-new.git\",\
                 \"hosting-enabled\":false}} | PHID-CDTL-one | {NOW}"
            )],
            vec![
                "github.com/mozilla/one-mirror".to_owned(),
                "github.com/mozilla/one-new".to_owned(),
            ],
        )
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', objectPHID, transactionType, oldValue, newValue) AS value \
             FROM `{ns}_repository`.repository_uritransaction ORDER BY id",
        )
        .await,
        [
            "PHID-RURI-1 | diffusion.uri.uri | \"https:\\/\\/github.com\\/mozilla\\/one.git\" | \
          \"https:\\/\\/github.com\\/mozilla\\/one-new.git\""
        ]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT statusType AS value FROM `{ns}_repository`.repository_statusmessage",
        )
        .await,
        ["fetch"]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT objectPHID AS value FROM `{ns}_worker`.worker_activetask",
        )
        .await,
        ["PHID-RURI-1"]
    );

    // Without an Observe URI, the repository is hosted.
    succeeds(
        &context,
        edit(json!(1), json!([{"type": "io", "value": "none"}])),
    )
    .await;
    assert_eq!(
        repository(&test_db).await.0,
        [format!(
            "{{\"tracking-enabled\":\"active\",\"remote-uri\":null,\"hosting-enabled\":true}} \
             | NULL | {NOW}"
        )]
    );

    // A new domain removes the credential; the default port goes.
    succeeds(
        &context,
        edit(
            json!(1),
            json!([
                {"type": "uri", "value": "ssh://git@GitLab.com:22/mozilla/one.git"},
                {"type": "io", "value": "observe"},
                {"type": "display", "value": "never"},
            ]),
        ),
    )
    .await;
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', uri, ioType, displayType, isDisabled, \
               IFNULL(credentialPHID, 'NULL'), dateModified) AS value \
             FROM `{ns}_repository`.repository_uri WHERE id = 1",
        )
        .await,
        [format!(
            "ssh://git@GitLab.com:22/mozilla/one.git | observe | never | 0 | NULL | {NOW}"
        )]
    );
    assert_eq!(
        repository(&test_db).await,
        (
            vec![format!(
                "{{\"tracking-enabled\":\"active\",\
                 \"remote-uri\":\"ssh:\\/\\/git@GitLab.com\\/mozilla\\/one.git\",\
                 \"hosting-enabled\":false}} | NULL | {NOW}"
            )],
            vec![
                "github.com/mozilla/one-mirror".to_owned(),
                "gitlab.com/mozilla/one".to_owned(),
            ],
        )
    );

    // Disabled URIs leave the index.
    succeeds(
        &context,
        edit(json!(2), json!([{"type": "disable", "value": true}])),
    )
    .await;
    assert_eq!(repository(&test_db).await.1, ["gitlab.com/mozilla/one"]);
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', transactionType, oldValue, newValue) AS value \
             FROM `{ns}_repository`.repository_uritransaction WHERE objectPHID = 'PHID-RURI-2'",
        )
        .await,
        ["diffusion.uri.disable | 0 | 1"]
    );

    // Validation errors.
    for (id, transaction, message) in [
        (
            1,
            json!({"type": "io", "value": "readwrite"}),
            "Value \"readwrite\" is not a valid IO setting for this URI. Available types for \
             this URI are: default, none, observe, mirror.",
        ),
        (
            2,
            json!({"type": "io", "value": "observe"}),
            "You can not set this URI to use Observe IO because another URI for this \
             repository is already configured in Observe IO mode. A repository can not \
             observe two different remotes simultaneously. Turn off IO for the other URI \
             first.",
        ),
        (
            1,
            json!({"type": "display", "value": "sometimes"}),
            "Value \"sometimes\" is not a valid display setting for this URI. Available types \
             for this URI are: default, always, never.",
        ),
        (
            1,
            json!({"type": "uri", "value": "file:///var/repo"}),
            "The URI protocol is unrecognized. It should begin with \"ssh://\", \"http://\", \
             \"https://\", \"git://\", \"svn://\", \"svn+ssh://\", or be in the form \
             \"git@domain.com:path\".",
        ),
        (
            1,
            json!({"type": "uri", "value": " https://github.com/a/b"}),
            "The remote URI has leading or trailing whitespace.",
        ),
    ] {
        assert_eq!(
            call(&context, edit(json!(id), json!([transaction.clone()]))).await,
            error(message),
            "{transaction}"
        );
    }
    assert_eq!(
        call(
            &context,
            edit(json!(1), json!([{"type": "disable", "value": "maybe"}]))
        )
        .await,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CORE",
            "Exception when processing transaction of type \"disable\": Error while reading \
             \"value\": Expected boolean (true or false), got something else."
        ))
    );

    // Left to PHP.
    for params in [
        edit(json!(4), json!([{"type": "io", "value": "none"}])),
        edit(json!(7), json!([{"type": "io", "value": "mirror"}])),
        edit(json!(6), json!([{"type": "io", "value": "mirror"}])),
        edit(json!(99), json!([{"type": "io", "value": "mirror"}])),
        edit(json!(1), json!([{"type": "io", "value": "observe"}])),
        edit(json!(1), json!([{"type": "uri", "value": ""}])),
        edit(
            json!(1),
            json!([{"type": "uri", "value": "https://host/x?y"}]),
        ),
        edit(json!(1), json!([{"type": "credential", "value": null}])),
        edit(
            json!(1),
            json!([{"type": "io", "value": "x"}, {"type": "display", "value": "y"}]),
        ),
        json!({"transactions": [
            {"type": "repository", "value": "PHID-REPO-one"},
            {"type": "uri", "value": "https://example.com/new"},
        ]}),
    ] {
        let response = call(&context, params.clone()).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{params}: {response:?}"
        );
    }

    test_db.drop_databases().await;
}
