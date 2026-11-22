//! `portal.search` against a test database.
//!
//! Bob sees the open portal (1), his own (3) and the archived one (4), not
//! the one nobody sees (2).

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pppppppppppppppppppppppppppp";

async fn query(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_dashboard::register(&mut registry);
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
        "portal.search",
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

async fn error(context: &Context, params: Value) -> (String, String) {
    match query(context, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info,
            ..
        }) => (code, error_info.unwrap_or_default()),
        other => panic!("{params}: expected an error, got {other:?}"),
    }
}

async fn is_fallback(context: &Context, params: Value) -> bool {
    matches!(query(context, params).await, Response::Fallback { .. })
}

async fn ids(context: &Context, params: Value) -> Vec<i64> {
    answer(context, params).await["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|row| row["id"].as_i64().expect("id"))
        .collect()
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "dashboard",
        "user",
        "conduit",
        "project",
        "policy",
        "spaces",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_dashboard`.dashboard_portal (id, phid, name, status, viewPolicy, \
               editPolicy, properties, dateCreated, dateModified) VALUES \
             (1, 'PHID-PRTL-1', 'Open', 'active', 'users', 'users', '{{}}', 10, 11), \
             (2, 'PHID-PRTL-2', 'Hidden', 'active', 'no-one', 'no-one', '{{}}', 20, 21), \
             (3, 'PHID-PRTL-3', 'Bob', 'active', 'PHID-USER-bob', 'admin', '[]', 30, 31), \
             (4, 'PHID-PRTL-4', 'Old', 'archived', 'users', 'users', '{{}}', 40, 41); \
             INSERT INTO `{{ns}}_dashboard`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PRTL-1', 41, 'PHID-PROJ-a', 1, 0), ('PHID-PRTL-1', 41, 'PHID-PROJ-b', 2, 0)"
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
async fn portal_search_answers_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(ids(&context, json!({})).await, [4, 3, 1]);
    assert_eq!(
        answer(&context, json!({"constraints": {"ids": [1, 2]}})).await,
        json!({
            "data": [{
                "id": 1,
                "type": "PRTL",
                "phid": "PHID-PRTL-1",
                "fields": {
                    "dateCreated": 10,
                    "dateModified": 11,
                    "policy": {"view": "users", "edit": "users"},
                },
                "attachments": {},
            }],
            "maps": {},
            "query": {"queryKey": null},
            "cursor": {"limit": 100, "after": null, "before": null, "order": null},
        })
    );
    // The newest edge first.
    assert_eq!(
        answer(
            &context,
            json!({"constraints": {"phids": ["PHID-PRTL-1"]}, "attachments": {"projects": true}})
        )
        .await["data"][0]["attachments"],
        json!({"projects": {"projectPHIDs": ["PHID-PROJ-b", "PHID-PROJ-a"]}})
    );
    assert_eq!(
        answer(
            &context,
            json!({"constraints": {"ids": [3]}, "attachments": {"projects": 1}})
        )
        .await["data"][0]["attachments"],
        json!({"projects": {"projectPHIDs": []}})
    );

    // Paging.
    let first = answer(&context, json!({"limit": 2})).await;
    assert_eq!(first["cursor"]["after"], json!("3"));
    assert_eq!(ids(&context, json!({"limit": 2, "after": "3"})).await, [1]);
    assert_eq!(ids(&context, json!({"limit": 1, "before": "1"})).await, [3]);
    assert_eq!(ids(&context, json!({"order": "oldest"})).await, [1, 3, 4]);
    assert_eq!(
        ids(&context, json!({"constraints": {"query": ""}})).await,
        [4, 3, 1]
    );

    // PHP's errors.
    assert_eq!(
        error(&context, json!({"constraints": {"statuses": ["active"]}})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Constraint \"statuses\" is not a valid constraint for this query.".to_owned()
        )
    );
    assert_eq!(
        error(&context, json!({"order": "name"})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Query \"PhabricatorDashboardPortalQuery\" does not support a builtin order \
             \"name\". Supported orders are: newest, created, oldest, relevance."
                .to_owned()
        )
    );

    // Left to PHP.
    for params in [
        json!({"constraints": {"query": "open"}}),
        json!({"constraints": {"projects": ["PHID-PROJ-a"]}}),
        json!({"order": "relevance"}),
        json!({"queryKey": "all"}),
    ] {
        assert!(is_fallback(&context, params.clone()).await, "{params}");
    }

    test_db.drop_databases().await;
}
