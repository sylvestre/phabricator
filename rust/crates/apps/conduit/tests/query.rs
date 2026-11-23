//! `conduit.query` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-cqcqcqcqcqcqcqcqcqcqcqcqcqcq";

async fn query(context: &Context) -> Value {
    let mut registry = Registry::default();
    phab_app_conduit::register(&mut registry);
    let http = HttpRequest {
        body: format!("api.token={TOKEN}"),
        ..HttpRequest::default()
    };
    match call::dispatch(context, &registry, "conduit.query", &http, Mode::Serve, 1)
        .await
        .response
    {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("expected an answer, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_the_methods_of_visible_applications() {
    let Some(test_db) =
        TestDatabase::create(&["conduit", "user", "policy", "spaces", "auth", "project"]).await
    else {
        return;
    };
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
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.uninstalled-applications": {"PhabricatorDashboardApplication": true},
        "phabricator.application-settings": {
            "PHID-APPS-PhabricatorHarbormasterApplication": {"policy": {"view": "admin"}},
        },
    }) {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web-7".to_owned(),
    };

    let result = query(&context).await;
    let methods = result.as_object().expect("a map");
    assert_eq!(
        methods["conduit.query"],
        json!({
            "description": "Returns the parameters of the Conduit methods.",
            "params": [],
            "return": "dict<dict>",
        })
    );
    assert_eq!(
        methods["diffusion.looksoon"]["params"],
        json!({
            "callsigns": "optional list<string> (deprecated)",
            "repositories": "optional list<string>",
            "urgency": "optional string",
        })
    );
    // Uninstalled applications and applications bob can't see.
    assert!(!methods.contains_key("dashboard.panel.edit"));
    assert!(!methods.contains_key("harbormaster.buildplan.edit"));
    assert!(methods.contains_key("differential.revision.edit"));

    test_db.drop_databases().await;
}
