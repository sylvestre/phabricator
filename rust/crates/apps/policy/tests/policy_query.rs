//! `policy.query` against a test database.

use phab_conduit::{Context, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pypypypypypypypypypypypypypy";

async fn query(context: &Context, params: Value) -> Response {
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
        "policy.query",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

#[tokio::test]
async fn describes_custom_policies() {
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
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_policy`.policy (id, phid, rules, defaultAction, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-PLCY-zz', '[{{\"action\":\"allow\",\"rule\":\"PhabricatorProjectsPolicyRule\",\"value\":[\"PHID-PROJ-a\"]}}]', 'deny', 1, 1), \
             (2, 'PHID-PLCY-aa', '[]', 'allow', 1, 1)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };

    // In ID order, whatever the order asked.
    let Response::Envelope(envelope) =
        query(&context, json!({"phids": ["PHID-PLCY-aa", "PHID-PLCY-zz"]})).await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        envelope.result,
        json!({
            "data": [
                {
                    "phid": "PHID-PLCY-zz",
                    "type": "custom",
                    "name": "Custom Policy",
                    "shortName": "Custom Policy",
                    "fullName": "Custom Policy",
                    "href": null,
                    "workflow": null,
                    "icon": "fa-certificate",
                    "default": "deny",
                    "rules": [
                        {"action": "allow", "rule": "PhabricatorProjectsPolicyRule",
                         "value": ["PHID-PROJ-a"]},
                    ],
                },
                {
                    "phid": "PHID-PLCY-aa",
                    "type": "custom",
                    "name": "Custom Policy",
                    "shortName": "Custom Policy",
                    "fullName": "Custom Policy",
                    "href": null,
                    "workflow": null,
                    "icon": "fa-certificate",
                    "default": "allow",
                    "rules": [],
                },
            ],
            "cursor": {"limit": 100, "after": null, "before": null},
        })
    );
    for (params, info) in [
        (json!({}), "PHIDs required"),
        (
            json!({"phids": ["PHID-PLCY-x", "PHID-PLCY-y"]}),
            "Unknown policies: PHID-PLCY-x, PHID-PLCY-y",
        ),
    ] {
        let Response::Envelope(envelope) = query(&context, params.clone()).await else {
            panic!("{params}: expected an error");
        };
        assert_eq!(
            (
                envelope.error_code.as_deref(),
                envelope.error_info.as_deref()
            ),
            (Some("ERR-INVALID-PARAMETER"), Some(info)),
            "{params}"
        );
    }
    for params in [
        json!({"phids": ["users"]}),
        json!({"phids": ["PHID-PLCY-aa", "PHID-PLCY-zz"], "limit": 1}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
