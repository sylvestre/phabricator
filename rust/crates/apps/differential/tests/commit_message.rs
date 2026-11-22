//! `differential.getcommitmessage` and `differential.parsecommitmessage`
//! against a test database. The expected answers follow
//! `DifferentialGetCommitMessageConduitAPIMethod`,
//! `DifferentialCommitMessageParser` and `PhabricatorObjectListQuery`.

// Each method's checks share one database, so they are one long function.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_core::hash::digest_for_index;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

/// Alice's token; she is not an administrator.
const ALICE: &str = "api-cmcmcmcmcmcmcmcmcmcmcmcmcmca";
/// Root's token, an administrator.
const ROOT: &str = "api-cmcmcmcmcmcmcmcmcmcmcmcmcmcr";

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

async fn call(context: &Context, method: &str, token: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": token});
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

async fn answer(context: &Context, method: &str, token: &str, params: Value) -> Value {
    match call(context, method, token, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn is_left_to_php(context: &Context, method: &str, token: &str, params: Value) -> bool {
    matches!(
        call(context, method, token, params).await,
        Response::Fallback(_)
    )
}

async fn message(context: &Context, params: Value) -> String {
    let result = answer(context, "differential.getcommitmessage", ALICE, params).await;
    result.as_str().expect("a string").to_owned()
}

async fn parse(context: &Context, token: &str, params: Value) -> Value {
    answer(context, "differential.parsecommitmessage", token, params).await
}

/// Users `(name, is disabled, is admin)`.
const USERS: &[(&str, i64, i64)] = &[
    ("alice", 0, 0),
    ("bob", 0, 0),
    ("carol", 0, 0),
    ("dave", 1, 0),
    ("root", 0, 1),
];

/// Projects `(id, PHID, view policy, primary slug, slugs)`.
const PROJECTS: &[(i64, &str, &str, &str, &[&str])] = &[
    (
        1,
        "PHID-PROJ-front",
        "users",
        "'frontend'",
        &["frontend", "front-end"],
    ),
    (2, "PHID-PROJ-secret", "no-one", "'secret'", &["secret"]),
    (3, "PHID-PROJ-noslug", "users", "NULL", &[]),
];

/// Revisions `(id, author, view policy, title, summary, test plan)`.
const REVISIONS: &[(i64, &str, &str, &str, &str, &str)] = &[
    (
        1,
        "alice",
        "users",
        "Fix the parser",
        "Parse more.\n\nDepends on D2",
        "Ran the tests.",
    ),
    (2, "alice", "users", "", "", ""),
    (
        3,
        "bob",
        "users",
        "Indent",
        "  indented\r\nsecond",
        "one line",
    ),
    (4, "bob", "no-one", "Hidden", "", ""),
];

/// Reviewers `(revision, reviewer, status)`, in creation order.
const REVIEWERS: &[(i64, &str, &str)] = &[
    (1, "PHID-USER-bob", "accepted"),
    (1, "PHID-USER-carol", "blocking"),
    (1, "PHID-PROJ-front", "added"),
    (1, "PHID-USER-dave", "resigned"),
];

/// Edges `(revision, type, destination, dateCreated)`: subscribers (21)
/// and projects (41).
const EDGES: &[(i64, i64, &str, i64)] = &[
    (1, 21, "PHID-USER-dave", 1),
    (1, 21, "PHID-USER-carol", 2),
    (1, 41, "PHID-PROJ-front", 1),
    (1, 41, "PHID-PROJ-secret", 2),
    (1, 41, "PHID-PROJ-noslug", 3),
];

/// Custom field storage `(revision, key, value)`.
const CUSTOM: &[(i64, &str, &str)] = &[
    (1, "differential:bugzilla-bug-id", "1234"),
    (3, "differential:bugzilla-bug-id", ""),
    (3, "phabricator:auditors", "[\"PHID-USER-bob\"]"),
];

fn config(values: Value) -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    if let Value::Object(map) = values {
        config.push_layer("more", map);
    }
    config
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    for (name, disabled, admin) in USERS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, \
                   dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
                   isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
                   isMailingList) VALUES \
                 ('PHID-USER-{name}', '{name}', '{name}', 1, 1, 'c', 0, {disabled}, {admin}, 1, \
                   1, 's', 0, 0)"
            ))
            .await;
    }
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', '{ALICE}', NULL, 1, 1), \
             ('PHID-USER-root', 'api', '{ROOT}', NULL, 1, 1)"
        ))
        .await;
    for (id, phid, policy, primary, slugs) in PROJECTS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
                   dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
                   hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
                   projectPathKey, properties, subtype, parentProjectPHID, milestoneNumber, \
                   primarySlug) VALUES \
                 ({id}, 'P{id}', '{phid}', 'PHID-USER-x', 1, 1, '0', '{policy}', 'users', \
                   'users', 'fa-x', 'blue', 'k', 0, 0, 0, 'p{id}', 0, 'k{id:03}', '{{}}', \
                   'default', NULL, NULL, {primary})"
            ))
            .await;
        for slug in *slugs {
            test_db
                .execute(&format!(
                    "INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, \
                       dateModified) VALUES ('{phid}', '{slug}', 1, 1)"
                ))
                .await;
        }
    }
    for (id, author, policy, title, summary, test_plan) in REVISIONS {
        let summary = summary.replace('\r', "\\r").replace('\n', "\\n");
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, \
                   status, summary, testPlan, authorPHID, dateCreated, dateModified, attached, \
                   mailKey, viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) \
                 VALUES ({id}, '{title}', 'PHID-DREV-{id}', 'needs-review', '{summary}', \
                   '{test_plan}', 'PHID-USER-{author}', {id}, {id}, '{{}}', 'k', '{policy}', \
                   'users', NULL, '{{}}', 'PHID-DIFF-{id}')"
            ))
            .await;
    }
    for (revision, reviewer, status) in REVIEWERS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
                   reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
                 ('PHID-DREV-{revision}', '{reviewer}', '{status}', 1, 1, '{{}}')"
            ))
            .await;
    }
    for (revision, edge_type, destination, created) in EDGES {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
                 ('PHID-DREV-{revision}', {edge_type}, '{destination}', {created}, 0)"
            ))
            .await;
    }
    for (revision, key, value) in CUSTOM {
        let index = digest_for_index(key.as_bytes());
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_customfieldstorage (objectPHID, \
                   fieldIndex, fieldValue) VALUES ('PHID-DREV-{revision}', '{index}', '{value}')"
            ))
            .await;
    }
    let context = Context {
        database: test_db.database().clone(),
        config: config(json!({})),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn renders_commit_messages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let d1 = "Fix the parser\n\n\
              Summary:\nParse more.\n\nDepends on D2\n\n\
              Test Plan: Ran the tests.\n\n\
              Reviewers: bob, carol!, #frontend, dave\n\n\
              Reviewed By: bob\n\n\
              Subscribers: carol, dave\n\n\
              Tags: #frontend, PHID-PROJ-secret, PHID-PROJ-noslug\n\n\
              Bug #: 1234\n\n\
              Differential Revision: http://phabricator.test/D1";
    assert_eq!(message(&context, json!({"revision_id": 1})).await, d1);
    assert_eq!(message(&context, json!({"revision_id": "1"})).await, d1);
    // Editing hides "Reviewed By"; no field of D1 is empty.
    let edited = d1.replace("Reviewed By: bob\n\n", "");
    for mode in ["edit", "create"] {
        assert_eq!(
            message(&context, json!({"revision_id": 1, "edit": mode})).await,
            edited
        );
    }

    let d2 = "<<Replace this line with your revision title>\n\n\
              Differential Revision: http://phabricator.test/D2";
    assert_eq!(message(&context, json!({"revision_id": 2})).await, d2);
    assert_eq!(
        message(&context, json!({"revision_id": 2, "edit": "edit"})).await,
        d2
    );
    assert_eq!(
        message(&context, json!({"revision_id": 2, "edit": "create"})).await,
        "<<Replace this line with your revision title>\n\n\
         Summary: \n\nTest Plan: \n\nReviewers: \n\nSubscribers: \n\nBug #: \n\n\
         Differential Revision: http://phabricator.test/D2"
    );

    // Line breaks are normalized; leading spaces put the value on its own
    // lines; an empty bug number is no field.
    assert_eq!(
        message(&context, json!({"revision_id": 3})).await,
        "Indent\n\nSummary:\n  indented\nsecond\n\nTest Plan: one line\n\nAuditors: bob\n\n\
         Differential Revision: http://phabricator.test/D3"
    );

    // Values of "fields" replace the revision's.
    assert_eq!(
        message(
            &context,
            json!({"revision_id": 2, "edit": "edit", "fields": {
                "title": "New title",
                "summary": "  starts with spaces",
                "reviewerPHIDs": ["PHID-USER-bob!", "PHID-PROJ-secret", "PHID-USER-bob"],
                "revisionID": 7,
                "bugzilla.bug-id": null,
                "reviewedByPHIDs": ["PHID-USER-carol"],
                "unknown": 5,
            }})
        )
        .await,
        "New title\n\nSummary:\n  starts with spaces\n\nReviewers: bob!, PHID-PROJ-secret\n\n\
         Differential Revision: http://phabricator.test/D7"
    );
    assert_eq!(
        message(&context, json!({"revision_id": 2, "fields": []})).await,
        d2
    );

    // Without Test Plan, with Revert Plan: custom fields listed in
    // differential.fields come first.
    let other = Context {
        config: config(json!({"differential.fields": {
            "differential:test-plan": {"disabled": true},
            "phabricator:revert-plan": {"disabled": false},
        }})),
        database: context.database.clone(),
        hostname: context.hostname.clone(),
    };
    assert_eq!(
        answer(
            &other,
            "differential.getcommitmessage",
            ALICE,
            json!({"revision_id": 3, "edit": "create"})
        )
        .await,
        json!(
            "Indent\n\nSummary:\n  indented\nsecond\n\nReviewers: \n\nSubscribers: \n\n\
             Revert Plan: \n\nAuditors: bob\n\nBug #: \n\n\
             Differential Revision: http://phabricator.test/D3"
        )
    );

    match call(
        &context,
        "differential.getcommitmessage",
        ALICE,
        json!({"revision_id": 99}),
    )
    .await
    {
        Response::Envelope(envelope) => {
            assert_eq!(envelope.error_code.as_deref(), Some("ERR_NOT_FOUND"));
            assert_eq!(
                envelope.error_info.as_deref(),
                Some("Revision was not found.")
            );
        }
        other @ Response::Fallback(_) => panic!("expected an error, got {other:?}"),
    }
    for params in [
        // The viewer can't see it.
        json!({"revision_id": 4}),
        // A new revision.
        json!({}),
        json!({"revision_id": 0, "edit": "create"}),
        json!({"revision_id": 1, "edit": true}),
        json!({"revision_id": 1, "fields": {"summary": 5}}),
        json!({"revision_id": 1, "fields": {"ccPHIDs": ["PHID-DREV-1"]}}),
        json!({"revision_id": 1, "fields": {"ccPHIDs": ["PHID-USER-ghost"]}}),
        json!({"revision_id": 1, "fields": null}),
    ] {
        assert!(
            is_left_to_php(
                &context,
                "differential.getcommitmessage",
                ALICE,
                params.clone()
            )
            .await,
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn parses_commit_messages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let corpus = "Fix the parser\n\n\
                  Summary: Parse more.\n\nDepends on D2\n\n\
                  Test Plan: Ran the tests.\n\n\
                  Reviewers: bob, carol!, #frontend\n\n\
                  Subscribers: dave, @Carol\n\n\
                  Tags: #frontend\n\n\
                  Differential Revision: http://phabricator.test/D1\n";
    // Objects found by monogram (#frontend, @Carol) come before those
    // found by username.
    let reviewers = json!(["PHID-PROJ-front", "PHID-USER-bob", "PHID-USER-carol!"]);
    assert_eq!(
        parse(&context, ALICE, json!({"corpus": corpus})).await,
        json!({
            "errors": [],
            "fields": {
                "title": "Fix the parser",
                "summary": "Parse more.\n\nDepends on D2",
                "testPlan": "Ran the tests.",
                "reviewerPHIDs": reviewers,
                "ccPHIDs": ["PHID-USER-carol", "PHID-USER-dave"],
                "phabricator:projects": ["PHID-PROJ-front"],
                "revisionID": 1,
            },
            "revisionIDFieldInfo": {"value": 1, "validDomain": "http://phabricator.test"},
            "transactions": [
                {"type": "title", "value": "Fix the parser"},
                {"type": "summary", "value": "Parse more.\n\nDepends on D2"},
                {"type": "testPlan", "value": "Ran the tests."},
                {"type": "reviewers.set", "value": [
                    "PHID-PROJ-front", "PHID-USER-bob", "blocking(PHID-USER-carol)",
                ]},
                {"type": "subscribers.set", "value": ["PHID-USER-carol", "PHID-USER-dave"]},
                {"type": "projects.set", "value": ["PHID-PROJ-front"]},
            ],
        })
    );

    // Aliases, a field given twice, unknown and invalid objects.
    let corpus = "Quick fix\n\
                  Reviewer: nobody, D12x\n\
                  CC: bob bob, PHID-USER-ghost\n\
                  Project: #secret, alice, PHID-PROJ-secret\n\
                  Bugzilla: #abc\n\
                  Tested:\n\
                  reviewers: carol";
    assert_eq!(
        parse(&context, ALICE, json!({"corpus": corpus})).await,
        json!({
            "errors": [
                "Field \"reviewers\" occurs twice in commit message!",
                "Error parsing field \"Reviewers\": The objects you have listed include objects \
                 which do not exist (nobody, D12x).",
                "Error parsing field \"Tags\": The objects you have listed include objects of \
                 the wrong type (alice) and objects which do not exist (#secret).",
                "Invalid or missing field \"Test Plan\": You must provide a test plan. Describe \
                 the actions you performed to verify the behavior of this change.",
                "Invalid or missing field \"Bug #\": Bugzilla Bug ID must be a valid bug number",
            ],
            "fields": {
                "title": "Quick fix",
                "ccPHIDs": ["PHID-USER-bob", "PHID-USER-ghost"],
                "bugzilla.bug-id": "#abc",
                "testPlan": "",
            },
            "revisionIDFieldInfo": {"value": null, "validDomain": "http://phabricator.test"},
            "transactions": [
                {"type": "title", "value": "Quick fix"},
                {"type": "subscribers.set", "value": ["PHID-USER-bob", "PHID-USER-ghost"]},
                {"type": "bugzilla.bug-id", "value": "#abc"},
                {"type": "testPlan", "value": ""},
            ],
        })
    );

    // A partial message: the default title is refused, "Reviewed By"
    // keeps what it finds.
    let corpus = "<<Replace this line with your revision title>\n\n\
                  Reviewed By: bob, ghost\n\
                  Differential Revision: D7";
    assert_eq!(
        parse(&context, ALICE, json!({"corpus": corpus, "partial": true})).await,
        json!({
            "errors": [
                "Error parsing field \"Title\": Replace the default title line with a \
                 human-readable revision title which describes the changes you are making.",
            ],
            "fields": {"reviewedByPHIDs": ["PHID-USER-bob"], "revisionID": 7},
            "revisionIDFieldInfo": {"value": 7, "validDomain": "http://phabricator.test"},
            "transactions": [],
        })
    );
    // Nothing parsed is an empty list.
    assert_eq!(
        parse(
            &context,
            ALICE,
            json!({"corpus": "<<Replace this line with your revision title>", "partial": 1})
        )
        .await["fields"],
        json!([])
    );

    assert_eq!(
        parse(&context, ALICE, json!({"corpus": ""})).await,
        json!({
            "errors": [
                "Invalid or missing field \"Title\": You must provide a revision title in the \
                 first line of your commit message.",
                "Invalid or missing field \"Test Plan\": You must provide a test plan. Describe \
                 the actions you performed to verify the behavior of this change.",
            ],
            "fields": {"title": ""},
            "revisionIDFieldInfo": {"value": null, "validDomain": "http://phabricator.test"},
            "transactions": [{"type": "title", "value": ""}],
        })
    );

    // Administrators' bug numbers are not checked with Bugzilla; revision
    // URIs of other installs are no revision.
    let corpus = "T\n\nlong summary\n\nBug #: 1234\nTags: #Front-End, PHID-PROJ-secret\n\
                  Differential Revision: https://elsewhere.test/D3\nTest Plan: x";
    assert_eq!(
        parse(&context, ROOT, json!({"corpus": corpus})).await,
        json!({
            "errors": [],
            "fields": {
                "title": "T",
                "bugzilla.bug-id": "1234",
                "phabricator:projects": ["PHID-PROJ-front", "PHID-PROJ-secret"],
                "revisionID": null,
                "testPlan": "x",
                "summary": "long summary",
            },
            "revisionIDFieldInfo": {"value": null, "validDomain": "http://phabricator.test"},
            "transactions": [
                {"type": "title", "value": "T"},
                {"type": "bugzilla.bug-id", "value": "1234"},
                {"type": "projects.set", "value": ["PHID-PROJ-front", "PHID-PROJ-secret"]},
                {"type": "testPlan", "value": "x"},
                {"type": "summary", "value": "long summary"},
            ],
        })
    );

    // Revert Plan is enabled and Test Plan is not a field; bug numbers
    // may be required.
    let other = Context {
        config: config(json!({
            "differential.fields": {
                "differential:test-plan": {"disabled": true},
                "phabricator:revert-plan": {"disabled": false},
            },
            "bugzilla.require_bugs": true,
        })),
        database: context.database.clone(),
        hostname: context.hostname.clone(),
    };
    assert_eq!(
        parse(
            &other,
            ALICE,
            json!({"corpus": "T\nRevert Plan: undo it\nTest Plan: x"})
        )
        .await,
        json!({
            "errors": [
                "Invalid or missing field \"Bug #\": Bugzilla Bug ID is required",
            ],
            "fields": {"title": "T", "revertPlan": "undo it\nTest Plan: x"},
            "revisionIDFieldInfo": {"value": null, "validDomain": "http://phabricator.test"},
            "transactions": [
                {"type": "title", "value": "T"},
                {"type": "revertPlan", "value": "undo it\nTest Plan: x"},
            ],
        })
    );

    let long = format!("{}\n", "x".repeat(251));
    for corpus in [
        "T\nReviewers: D12".to_owned(),
        "T\nReviewers: 123".to_owned(),
        "T\nSubscribers: PHID-DREV-1".to_owned(),
        "T\nTags: abcdef1".to_owned(),
        "T\r\nSummary: x".to_owned(),
        "T\nDifferential Revision: http://u@phabricator.test/D1".to_owned(),
        long,
    ] {
        assert!(
            is_left_to_php(
                &context,
                "differential.parsecommitmessage",
                ALICE,
                json!({"corpus": corpus, "partial": true})
            )
            .await,
            "{corpus}"
        );
    }
    // Checked with Bugzilla.
    assert!(
        is_left_to_php(
            &context,
            "differential.parsecommitmessage",
            ALICE,
            json!({"corpus": "T\nBug #: 1234"})
        )
        .await
    );
    // Refusing the default title, then validating it.
    assert!(
        is_left_to_php(
            &context,
            "differential.parsecommitmessage",
            ALICE,
            json!({"corpus": "<<Replace this line with your revision title>"})
        )
        .await
    );

    test_db.drop_databases().await;
}
