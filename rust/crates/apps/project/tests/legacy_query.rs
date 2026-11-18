//! `project.query` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pqpqpqpqpqpqpqpqpqpqpqpqpqpq";

async fn query(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_project::register(&mut registry);
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
        "project.query",
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

/// The names of the projects found, in order.
async fn names(context: &Context, params: Value) -> Vec<String> {
    let result = answer(context, params).await;
    match &result["data"] {
        Value::Object(data) => data
            .values()
            .map(|project| project["name"].as_str().expect("name").to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Projects `(id, name, status, view policy, color, slug)`.
const PROJECTS: &[(i64, &str, &str, &str, &str, &str)] = &[
    (1, "Zeta", "0", "users", "blue", "zeta"),
    (2, "Alpha", "100", "users", "red", "alpha"),
    (3, "Secret", "0", "admin", "blue", "secret"),
    (4, "Mid", "0", "users", "blue", "mid_team"),
];

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&["project", "user", "policy", "spaces", "conduit"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PROJ-4', 60, 'PHID-USER-bob', 1, 0)"
        ))
        .await;
    for (id, name, status, view, color, slug) in PROJECTS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
                   dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, \
                   mailKey, hasWorkboard, hasMilestones, hasSubprojects, projectPath, \
                   projectDepth, projectPathKey, properties, subtype, parentProjectPHID, \
                   milestoneNumber, primarySlug) VALUES \
                 ({id}, '{name}', 'PHID-PROJ-{id}', 'PHID-USER-x', {id}0, {id}1, '{status}', \
                   '{view}', 'users', 'users', 'project', '{color}', 'k', 0, 0, 0, 'p{id}', 0, \
                   'k{id:03}', '{{}}', 'default', NULL, NULL, '{slug}'); \
                 INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, \
                   dateModified) VALUES ('PHID-PROJ-{id}', '{slug}', 1, 1)"
            ))
            .await;
    }
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn describes_projects() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        answer(&context, json!({"slugs": ["Mid Team"]})).await,
        json!({
            "data": {
                "PHID-PROJ-4": {
                    "id": "4",
                    "phid": "PHID-PROJ-4",
                    "name": "Mid",
                    "profileImagePHID": null,
                    "icon": "project",
                    "color": "blue",
                    "members": ["PHID-USER-bob"],
                    "slugs": ["mid_team"],
                    "dateCreated": "40",
                    "dateModified": "41",
                },
            },
            "slugMap": {"Mid Team": "PHID-PROJ-4"},
            "cursor": {"limit": 100, "after": null, "before": null},
        })
    );
    // PHP's empty arrays.
    assert_eq!(
        answer(&context, json!({"names": ["Nope"]})).await,
        json!({"data": [], "slugMap": [], "cursor": {"limit": 100, "after": null, "before": null}})
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn filters_by_name_order() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // By name; the secret project is hidden.
    assert_eq!(names(&context, json!({})).await, ["Alpha", "Mid", "Zeta"]);
    assert_eq!(
        names(&context, json!({"status": "status-active"})).await,
        ["Mid", "Zeta"]
    );
    assert_eq!(
        names(&context, json!({"status": "status-closed"})).await,
        ["Alpha"]
    );
    assert_eq!(
        names(&context, json!({"colors": ["blue"], "ids": [1, 3, 4]})).await,
        ["Mid", "Zeta"]
    );
    assert_eq!(
        names(&context, json!({"members": ["PHID-USER-bob"]})).await,
        ["Mid"]
    );
    let first = answer(&context, json!({"limit": 1})).await;
    assert_eq!(
        first["cursor"],
        json!({"limit": 1, "after": "2", "before": null})
    );
    for params in [json!({"icons": ["project"]}), json!({"offset": 1})] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
