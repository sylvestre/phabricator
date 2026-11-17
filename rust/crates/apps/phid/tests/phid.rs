//! `phid.query`, `phid.info` and `phid.lookup` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pppppppppppppppppppppppppppp";

const DATABASES: &[&str] = &[
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

async fn query(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_phid::register(&mut registry);
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
        method,
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match query(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

fn config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    config
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bot', 'phab-bot', '', 1, 1, 'c', 1, 1, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (phid, name, callsign, repositorySlug, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             ('PHID-REPO-vct', 'Version Control Tools', 'VCT', 'vct', 'hg', \
               '{{\"tracking-enabled\":\"active\"}}', 1, 1, 'users', 'admin', 'admin'), \
             ('PHID-REPO-old', 'Old', 'OLD', NULL, 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin'), \
             ('PHID-REPO-secret', 'Secret', 'SEC', NULL, 'hg', '{{}}', 1, 1, 'admin', 'admin', \
               'admin'); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Fix it', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', NULL, '{{}}', ''), \
             (2, 'Gone', 'PHID-DREV-closed', 'abandoned', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', ''), \
             (3, 'Hidden', 'PHID-DREV-hidden', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'admin', 'admin', NULL, '{{}}', ''); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (1, 'PHID-DIFF-open', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (2, 'PHID-DIFF-hidden', 3, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (3, 'PHID-DIFF-alone', NULL, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, parentProjectPHID, milestoneNumber, \
               primarySlug) VALUES \
             (7, 'Build Reviewers', 'PHID-PROJ-build', 'PHID-USER-x', 1, 1, '0', 'users', 'users', \
               'users', 'fa-x', 'blue', 'k', 0, 0, 0, 'p0', 0, 'k000', '{{}}', 'default', NULL, \
               NULL, 'build_reviewers'), \
             (8, 'Archived', 'PHID-PROJ-archived', 'PHID-USER-x', 1, 1, '100', 'users', 'users', \
               'users', 'fa-x', 'blue', 'k', 0, 0, 0, 'p1', 0, 'k001', '{{}}', 'default', NULL, \
               NULL, NULL), \
             (9, 'Secret', 'PHID-PROJ-secret', 'PHID-USER-x', 1, 1, '0', 'admin', 'users', \
               'users', 'fa-x', 'blue', 'k', 0, 0, 0, 'p2', 0, 'k002', '{{}}', 'default', NULL, \
               NULL, 'secret'); \
             INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, \
               dateModified) VALUES \
             ('PHID-PROJ-build', 'build_reviewers', 1, 1), ('PHID-PROJ-build', 'builders', 1, 1), \
             ('PHID-PROJ-secret', 'secret', 1, 1)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: config(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

fn handle(
    phid: &str,
    uri: &str,
    type_name: &str,
    name: &str,
    full_name: &str,
    status: &str,
) -> Value {
    json!({
        "phid": phid,
        "uri": format!("http://phabricator.test{uri}"),
        "typeName": type_name,
        "type": &phid[5..9],
        "name": name,
        "fullName": full_name,
        "status": status,
    })
}

fn bob() -> Value {
    handle(
        "PHID-USER-bob",
        "/p/bob/",
        "User",
        "bob",
        "bob (Bob)",
        "open",
    )
}

fn revision() -> Value {
    handle(
        "PHID-DREV-open",
        "/D1",
        "Differential Revision",
        "D1",
        "D1: Fix it",
        "open",
    )
}

fn project() -> Value {
    handle(
        "PHID-PROJ-build",
        "/tag/build_reviewers/",
        "Project",
        "Build Reviewers",
        "Build Reviewers",
        "open",
    )
}

#[tokio::test]
async fn queries_handles_by_phid() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        answer(
            &context,
            "phid.query",
            json!({"phids": [
                "PHID-USER-bob", "PHID-USER-bot", "PHID-USER-nobody",
                "PHID-DREV-open", "PHID-DREV-closed", "PHID-DREV-hidden", "PHID-DREV-open",
            ]})
        )
        .await,
        json!({
            "PHID-USER-bob": bob(),
            "PHID-USER-bot": handle(
                "PHID-USER-bot", "/p/phab-bot/", "User", "phab-bot", "phab-bot", "open"
            ),
            "PHID-DREV-open": revision(),
            "PHID-DREV-closed": handle(
                "PHID-DREV-closed", "/D2", "Differential Revision", "D2", "D2: Gone", "closed"
            ),
        })
    );
    // A diff follows its revision, or its own policy without one.
    assert_eq!(
        answer(
            &context,
            "phid.query",
            json!({"phids": ["PHID-DIFF-open", "PHID-DIFF-hidden", "PHID-DIFF-alone"]})
        )
        .await,
        json!({
            "PHID-DIFF-open": handle(
                "PHID-DIFF-open", "/differential/diff/1/", "Differential Diff", "Diff 1",
                "Diff 1", "open"
            ),
            "PHID-DIFF-alone": handle(
                "PHID-DIFF-alone", "/differential/diff/3/", "Differential Diff", "Diff 3",
                "Diff 3", "open"
            ),
        })
    );
    assert_eq!(
        answer(
            &context,
            "phid.query",
            json!({"phids": ["PHID-PROJ-build", "PHID-PROJ-archived", "PHID-PROJ-secret"]})
        )
        .await,
        json!({
            "PHID-PROJ-build": project(),
            "PHID-PROJ-archived": handle(
                "PHID-PROJ-archived", "/project/view/8/", "Project", "Archived", "Archived",
                "closed"
            ),
        })
    );
    assert_eq!(
        answer(
            &context,
            "phid.query",
            json!({"phids": ["PHID-REPO-vct", "PHID-REPO-old", "PHID-REPO-secret"]})
        )
        .await,
        json!({
            "PHID-REPO-vct": handle(
                "PHID-REPO-vct", "/source/vct/", "Repository", "rVCT",
                "rVCT Version Control Tools", "open"
            ),
            "PHID-REPO-old": handle(
                "PHID-REPO-old", "/diffusion/OLD/", "Repository", "rOLD", "rOLD Old", "closed"
            ),
        })
    );
    // PHP's empty array.
    assert_eq!(
        answer(
            &context,
            "phid.query",
            json!({"phids": ["PHID-DREV-hidden"]})
        )
        .await,
        json!([])
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn describes_one_object() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        answer(&context, "phid.info", json!({"phid": "PHID-DREV-open"})).await,
        revision()
    );
    for phid in ["PHID-DREV-hidden", "PHID-DREV-nope"] {
        let response = query(&context, "phid.info", json!({"phid": phid})).await;
        let Response::Envelope(envelope) = response else {
            panic!("{phid}: expected an error, got {response:?}");
        };
        assert_eq!(envelope.error_code.as_deref(), Some("ERR-BAD-PHID"));
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn looks_up_names() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        answer(
            &context,
            "phid.lookup",
            json!({"names": [
                "D1", "d1", "D3", "D99", "@bob", "@BOB", "@nobody",
                "#build_reviewers", "#Builders", "#secret", "#none",
                "PHID-PROJ-build", "PHID-DREV-hidden",
            ]})
        )
        .await,
        json!({
            "D1": revision(),
            "d1": revision(),
            "@bob": bob(),
            "@BOB": bob(),
            "#build_reviewers": project(),
            "#Builders": project(),
            "PHID-PROJ-build": project(),
        })
    );
    assert_eq!(
        answer(&context, "phid.lookup", json!({"names": ["D3"]})).await,
        json!([])
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    for (method, params) in [
        ("phid.query", json!({"phids": ["PHID-TASK-1"]})),
        ("phid.query", json!({"phids": "PHID-USER-bob"})),
        ("phid.info", json!({"phid": "PHID-CMIT-1"})),
        ("phid.lookup", json!({"names": ["T1"]})),
        ("phid.lookup", json!({"names": ["rVCT"]})),
        ("phid.lookup", json!({"names": ["#two words"]})),
    ] {
        assert!(
            matches!(
                query(&context, method, params.clone()).await,
                Response::Fallback(_)
            ),
            "{method} {params}"
        );
    }

    test_db.drop_databases().await;
}
