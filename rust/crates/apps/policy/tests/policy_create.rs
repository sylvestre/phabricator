//! `policy.create` against a test database.

// Many scenarios on one database: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pcpcpcpcpcpcpcpcpcpcpcpcpcpc";

async fn create(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_policy::register(&mut registry);
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
        "policy.create",
        &http,
        Mode::Serve,
        1_700_000_000,
    )
    .await
    .response
}

async fn invalid(context: &Context, params: Value) -> String {
    match create(context, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info: Some(info),
            ..
        }) if code == "ERR-INVALID-PARAMETER" => info,
        other => panic!("{params}: expected an error, got {other:?}"),
    }
}

async fn policies(test_db: &TestDatabase) -> Vec<String> {
    let sql = format!(
        "SELECT CONCAT_WS(' ', phid, rules, defaultAction, dateCreated, dateModified) AS line \
         FROM `{}_policy`.policy ORDER BY id",
        test_db.database().namespace()
    );
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("the policies")
        .iter()
        .map(|row| phab_db::rows::text(row, "line").expect("a line"))
        .collect()
}

fn context(test_db: &TestDatabase, settings: Value) -> Context {
    let mut config = Config::default();
    if let Value::Object(map) = settings {
        config.push_layer("test", map);
    }
    Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    }
}

#[tokio::test]
async fn creates_custom_policies() {
    let Some(test_db) =
        TestDatabase::create(&["policy", "user", "conduit", "project", "spaces"]).await
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
    let context = context(&test_db, json!({}));
    let rule = |action: &str, class: &str, value: Value| json!({"action": action, "rule": class, "value": value});
    let reviewers = rule(
        "allow",
        "PhabricatorDifferentialReviewersPolicyRule",
        Value::Null,
    );

    assert_eq!(
        invalid(&context, json!({"default": "deny", "policy": [reviewers]})).await,
        "objectType is required."
    );
    assert_eq!(
        invalid(&context, json!({"objectType": "0", "policy": []})).await,
        "objectType is required."
    );
    assert_eq!(
        invalid(
            &context,
            json!({"objectType": "DREV", "default": "deny", "policy": [{"rule": "x"}]})
        )
        .await,
        "Invalid action ''!"
    );
    assert_eq!(
        invalid(
            &context,
            json!({
                "objectType": "DREV",
                "policy": [
                    reviewers,
                    rule("allow", "PhabricatorProjectMembersPolicyRule", Value::Null),
                ],
            })
        )
        .await,
        "Invalid rule class 'PhabricatorProjectMembersPolicyRule'!"
    );
    assert_eq!(
        invalid(
            &context,
            json!({
                "objectType": "PROJ",
                "default": "deny",
                "policy": [rule("allow", "PhabricatorUsersPolicyRule", json!([]))],
            })
        )
        .await,
        "Rules do not have any effect."
    );
    for params in [
        json!({"objectType": "TASK", "default": "deny", "policy": [reviewers]}),
        json!({"objectType": "DREV", "default": "deny", "policy": {"0": reviewers}}),
        json!({
            "objectType": "DREV",
            "default": "deny",
            "policy": [rule("allow", "PhabricatorUsersPolicyRule", json!("PHID-USER-bob"))],
        }),
        json!({"objectType": "DREV", "policy": [reviewers]}),
        json!({"objectType": "DREV", "default": "d".repeat(33), "policy": [reviewers]}),
    ] {
        let response = create(&context, params.clone()).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{params}: expected PHP to answer, got {response:?}"
        );
    }
    assert_eq!(policies(&test_db).await, Vec::<String>::new());

    let Response::Envelope(envelope) = create(
        &context,
        json!({
            "objectType": "DREV",
            "default": "deny",
            "policy": [
                rule("allow", "PhabricatorUsersPolicyRule", json!(["PHID-USER-bob"])),
                rule("deny", "PhabricatorProjectsPolicyRule", json!([])),
                reviewers,
                {"action": "allow", "rule": "PhabricatorAdministratorsPolicyRule", "extra": 1},
            ],
        }),
    )
    .await
    else {
        panic!("expected an answer");
    };
    let phid = envelope.result["phid"].as_str().expect("a PHID").to_owned();
    assert!(phid.starts_with("PHID-PLCY-"));
    assert_eq!(
        envelope.result,
        json!({
            "phid": phid,
            "name": "Custom Policy",
            "full": "Custom Policy",
            "icon": "fa-certificate",
        })
    );
    assert_eq!(
        policies(&test_db).await,
        vec![format!(
            "{phid} [{{\"action\":\"allow\",\"rule\":\"PhabricatorUsersPolicyRule\",\"value\":[\"PHID-USER-bob\"]}},\
             {{\"action\":\"allow\",\"rule\":\"PhabricatorDifferentialReviewersPolicyRule\",\"value\":null}},\
             {{\"action\":\"allow\",\"rule\":\"PhabricatorAdministratorsPolicyRule\",\"value\":null}}] \
             deny 1700000000 1700000000"
        )]
    );

    // Without Differential, revisions are no installed type.
    let uninstalled = context_with_differential_uninstalled(&test_db);
    let response = create(
        &uninstalled,
        json!({"objectType": "DREV", "default": "deny", "policy": [reviewers]}),
    )
    .await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");
    assert_eq!(policies(&test_db).await.len(), 1);
}

fn context_with_differential_uninstalled(test_db: &TestDatabase) -> Context {
    context(
        test_db,
        json!({
            "phabricator.uninstalled-applications": {"PhabricatorDifferentialApplication": true}
        }),
    )
}
