//! The Herald pages against one test database.
//!
//! Root (an administrator) wrote the global rules; Alice and the disabled
//! Gone wrote personal ones. D1 and the CI project are visible to users;
//! D2 and the Hush project to no one. Transcript 1 is of D1, transcript 2
//! of D2, transcript 3 of D1 after garbage collection, and transcript 4 of
//! D1 through an adapter the pages don't render.

// The checks share one database, so each page's are one long function.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_app_herald::engine::{self, ActionRecord, Applied, Condition, Subject};
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::phpser::Value as Php;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

const ALICE: &str = "phsid=alice-token";
const BOB: &str = "phsid=bob-token";
const ROOT: &str = "phsid=root-token";

struct State {
    database: Database,
    config: Config,
    remarkup: phab_remarkup::Engine,
}

impl WebState for State {
    fn database(&self) -> &Database {
        &self.database
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn now(&self) -> i64 {
        NOW
    }

    fn remarkup(&self) -> &phab_remarkup::Engine {
        &self.remarkup
    }

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        None
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

fn rule(id: &str, author: &str, all: bool, conditions: Vec<Condition>) -> engine::Rule {
    engine::Rule {
        id: id.to_owned(),
        phid: format!("PHID-HRUL-{id}"),
        name: if id == "1" { "Tag CI" } else { "Alice reviews" }.to_owned(),
        author_phid: author.to_owned(),
        rule_type: if id == "1" { "global" } else { "personal" }.to_owned(),
        must_match_all: all,
        config_version: 38,
        conditions,
        actions: Vec::new(),
        author_valid: true,
        author_can_view: true,
        repetition_policy: "every".to_owned(),
        applied_before: false,
    }
}

fn condition(id: &str, field: &str, test: &str, value: Php) -> Condition {
    Condition {
        id: id.to_owned(),
        field: field.to_owned(),
        condition: test.to_owned(),
        value,
    }
}

/// The columns of transcript 1, as the Rust engine writes them: rules 1
/// and 2 match D1, rule 1 tags it and subscribes Bob, rule 2 declines to
/// add Alice, D1's author, as reviewer.
fn transcript_columns() -> [String; 4] {
    let rules = vec![
        rule(
            "1",
            "PHID-USER-root",
            true,
            vec![
                condition(
                    "1",
                    "differential.revision.title",
                    "contains",
                    Php::str("zz<b>"),
                ),
                condition(
                    "2",
                    "revision.status",
                    "isany",
                    Php::list([Php::str("accepted"), Php::str("draft")]),
                ),
                condition(
                    "3",
                    "projects",
                    "any",
                    Php::list([Php::str("PHID-PROJ-ci"), Php::str("PHID-PROJ-hush")]),
                ),
            ],
        ),
        rule(
            "2",
            "PHID-USER-alice",
            false,
            vec![condition("4", "cc", "exists", Php::Null)],
        ),
    ];
    let mut rules = rules;
    rules[0].actions = vec![
        ActionRecord {
            action: "projects.add".to_owned(),
            target: Php::list([Php::str("PHID-PROJ-ci")]),
        },
        ActionRecord {
            action: "subscribers.add".to_owned(),
            target: Php::list([Php::str("PHID-USER-bob")]),
        },
    ];
    rules[1].actions = vec![ActionRecord {
        action: "differential.reviewers.self.add".to_owned(),
        target: Php::Null,
    }];
    let subject = Subject {
        phid: "PHID-DREV-1".to_owned(),
        content_type: "differential".to_owned(),
        name: "Open".to_owned(),
        applied_transaction_phids: Some(vec!["PHID-XACT-DREV-1".to_owned()]),
    };
    let values = vec![
        (
            "differential.revision.title".to_owned(),
            Php::str("zz<b> thing"),
        ),
        ("revision.status".to_owned(), Php::str("accepted")),
        (
            "projects".to_owned(),
            Php::list([Php::str("PHID-PROJ-ci"), Php::str("PHID-PROJ-hush")]),
        ),
        ("cc".to_owned(), Php::list([Php::str("PHID-USER-bob")])),
    ];
    let evaluation = engine::evaluate(&rules, &subject, &values);
    assert_eq!(evaluation.effects.len(), 3);
    // Personal rules run first.
    let applied: Vec<Applied> = evaluation
        .effects
        .iter()
        .map(|effect| {
            let (kind, phid) = match effect.action.as_str() {
                "projects.add" => ("do.add-projects", "PHID-PROJ-ci"),
                "subscribers.add" => ("do.subscribed", "PHID-USER-bob"),
                _ => ("do.authors", "PHID-USER-alice"),
            };
            (effect, (kind, phid))
        })
        .map(|(effect, (kind, phid))| Applied {
            effect: effect.clone(),
            applied: kind != "do.authors",
            reasons: vec![(kind.to_owned(), Php::list([Php::str(phid)]))],
            standard: false,
        })
        .collect();
    let row = evaluation.transcript(&subject, &applied);
    [
        hex::encode(row.object_transcript),
        hex::encode(row.rule_transcripts),
        hex::encode(row.condition_transcripts),
        hex::encode(row.apply_transcripts),
    ]
}

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user",
        "auth",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
        "file",
        "harbormaster",
        "herald",
        "search",
    ])
    .await?;
    let [object, rules, conditions, applied] = transcript_columns();
    let commit_object = hex::encode(phab_db::phpser::serialize(&Php::Object {
        class: "HeraldObjectTranscript".to_owned(),
        properties: vec![
            (phab_db::phpser::protected("phid"), Php::str("PHID-DREV-1")),
            (phab_db::phpser::protected("type"), Php::str("commit")),
        ],
    }));
    let empty = hex::encode(phab_db::phpser::serialize(&Php::list([])));
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0), \
             ('PHID-USER-gone', 'gone', 'Gone', 1, 1, 'c', 0, 1, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-bob', 'web', '{bob}', 1, {expires}, 'PHID-SSSN-2', 0), \
             ('PHID-USER-root', 'web', '{root}', 1, {expires}, 'PHID-SSSN-3', 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', '{{}}', 'PHID-DIFF-1'), \
             (2, 'Hidden', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-root', 1, 1, '{{}}', \
               'k', 'no-one', 'no-one', '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_project`.project (name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             ('CI', 'PHID-PROJ-ci', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', 'red', 'k', \
               0, 0, 0, 'a', 0, 'k001', '{{}}', 'default', 'ci'), \
             ('Hush', 'PHID-PROJ-hush', 'x', 1, 1, '0', 'no-one', 'users', 'users', 'fa-x', 'red', \
               'k', 0, 0, 0, 'b', 0, 'k002', '{{}}', 'default', 'hush'); \
             INSERT INTO `{{ns}}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, ruleType, \
               phid, isDisabled, triggerObjectPHID) VALUES \
             (1, 'Tag CI', 'PHID-USER-root', 'differential', 1, 38, 1, 1, 'every', 'global', \
               'PHID-HRUL-1', 0, NULL), \
             (2, 'Alice reviews', 'PHID-USER-alice', 'differential', 0, 38, 1, 1, 'first', \
               'personal', 'PHID-HRUL-2', 0, NULL), \
             (3, 'Ghost rule', 'PHID-USER-gone', 'differential', 1, 38, 1, 1, 'change', \
               'personal', 'PHID-HRUL-3', 0, NULL), \
             (4, 'Off', 'PHID-USER-root', 'differential', 1, 38, 1, 1, 'every', 'global', \
               'PHID-HRUL-4', 1, NULL), \
             (5, 'Commits', 'PHID-USER-root', 'commit', 1, 38, 1, 1, 'every', 'global', \
               'PHID-HRUL-5', 0, NULL), \
             (6, 'Custom', 'PHID-USER-root', 'differential', 1, 38, 1, 1, 'every', 'global', \
               'PHID-HRUL-6', 0, NULL), \
             (7, 'Hushed object', 'PHID-USER-root', 'differential', 1, 38, 1, 1, 'every', \
               'object', 'PHID-HRUL-7', 0, 'PHID-PROJ-hush'), \
             (8, 'CI object', 'PHID-USER-root', 'differential', 1, 38, 1, 1, 'every', \
               'object', 'PHID-HRUL-8', 0, 'PHID-PROJ-ci'); \
             INSERT INTO `{{ns}}_herald`.herald_condition (id, ruleID, fieldName, fieldCondition, \
               value) VALUES \
             (1, 1, 'differential.revision.title', 'contains', '\"zz<b>\"'), \
             (2, 1, 'revision.status', 'isany', '[\"accepted\",\"draft\"]'), \
             (3, 1, 'projects', 'any', '[\"PHID-PROJ-ci\",\"PHID-PROJ-hush\"]'), \
             (4, 2, 'cc', 'exists', 'null'), \
             (5, 3, 'always', 'unconditionally', 'null'), \
             (6, 4, 'differential.revision.title', 'contains', '\"x\"'), \
             (7, 6, 'herald.custom.x', 'is', '\"y\"'); \
             INSERT INTO `{{ns}}_herald`.herald_action (id, ruleID, action, target) VALUES \
             (1, 1, 'projects.add', '[\"PHID-PROJ-ci\"]'), \
             (2, 1, 'subscribers.add', '[\"PHID-USER-bob\"]'), \
             (3, 2, 'differential.reviewers.self.add', 'null'), \
             (4, 3, 'email.self', 'null'), \
             (5, 4, 'nothing', 'null'), \
             (6, 6, 'nothing', 'null'); \
             INSERT INTO `{{ns}}_herald`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-HRUL-1', 21, 'PHID-USER-bob', 2, 0), \
             ('PHID-HRUL-1', 21, 'PHID-USER-alice', 1, 0); \
             INSERT INTO `{{ns}}_herald`.herald_ruletransaction (phid, authorPHID, objectPHID, \
               viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, \
               newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-HRUL-1', 'PHID-USER-root', 'PHID-HRUL-1', 'users', 'users', NULL, 0, \
               'core:create', 'null', 'null', '{{}}', '{{\"core.create\":true}}', 10, 10), \
             ('PHID-XACT-HRUL-2', 'PHID-USER-root', 'PHID-HRUL-1', 'users', 'users', NULL, 0, \
               'herald:name', '\"\"', '\"Tag\"', '{{}}', '{{\"core.create\":true}}', 10, 10), \
             ('PHID-XACT-HRUL-3', 'PHID-USER-root', 'PHID-HRUL-1', 'users', 'users', NULL, 0, \
               'herald:name', '\"Tag\"', '\"Tag CI\"', '{{}}', '{{}}', 20, 20), \
             ('PHID-XACT-HRUL-4', 'PHID-USER-root', 'PHID-HRUL-1', 'users', 'users', NULL, 0, \
               'herald:edit', '{{\"match_all\":true}}', '{{\"match_all\":false}}', '{{}}', '{{}}', \
               30, 30), \
             ('PHID-XACT-HRUL-5', 'PHID-USER-root', 'PHID-HRUL-1', 'users', 'users', NULL, 0, \
               'herald:disable', 'false', 'true', '{{}}', '{{}}', 40, 40), \
             ('PHID-XACT-HRUL-6', 'PHID-USER-root', 'PHID-HRUL-4', 'users', 'users', NULL, 0, \
               'core:subscribers', '[]', '[]', '{{}}', '{{}}', 40, 40); \
             INSERT INTO `{{ns}}_herald`.herald_transcript (id, phid, time, host, duration, \
               objectPHID, dryRun, objectTranscript, ruleTranscripts, conditionTranscripts, \
               applyTranscripts, garbageCollected) VALUES \
             (1, 'PHID-HLXS-1', {NOW}, 'web1', 0.0123, 'PHID-DREV-1', 0, UNHEX('{object}'), \
               UNHEX('{rules}'), UNHEX('{conditions}'), UNHEX('{applied}'), 0), \
             (2, 'PHID-HLXS-2', {NOW}, 'web1', 0.5, 'PHID-DREV-2', 0, UNHEX('{object}'), \
               UNHEX('{rules}'), UNHEX('{conditions}'), UNHEX('{applied}'), 0), \
             (3, 'PHID-HLXS-3', {NOW}, 'web1', 1.5, 'PHID-DREV-1', 1, '', '', '', '', 1), \
             (4, 'PHID-HLXS-4', {NOW}, 'web1', 0.0, 'PHID-DREV-1', 0, UNHEX('{commit_object}'), \
               UNHEX('{empty}'), UNHEX('{empty}'), UNHEX('{empty}'), 0); \
             INSERT INTO `{{ns}}_search`.search_namedquery (userPHID, engineClassName, queryName, \
               queryKey, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'HeraldRuleSearchEngine', 'Mine', 'abc', 1, 1)",
            alice = session_key("alice-token"),
            bob = session_key("bob-token"),
            root = session_key("root-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

async fn get(test_db: &TestDatabase, path: &str, cookie: Option<&str>) -> Response {
    let config = Config::default();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn page(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, Some(cookie)).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{path} as {cookie}: {reason:?} {text}"
    );
    text
}

async fn status(test_db: &TestDatabase, path: &str, cookie: &str) -> StatusCode {
    get(test_db, path, Some(cookie)).await.status()
}

async fn fallback(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, Some(cookie)).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn rules(test_db: &TestDatabase) {
    let page = page(test_db, "/H1", ALICE).await;
    assert!(page.contains("Tag CI</h1>"), "{page}");
    assert!(page.contains("<dt>Rule Type</dt><dd>Global</dd>"), "{page}");
    assert!(
        page.contains("<dt>Applies To</dt><dd>Differential Revisions</dd>"),
        "{page}"
    );
    assert!(!page.contains("<dt>Author</dt>"), "{page}");
    assert!(page.contains("When all of these conditions are met:"));
    assert!(
        page.contains("Revision title contains zz&lt;b&gt;"),
        "{page}"
    );
    assert!(page.contains("Revision status is any of Accepted, Draft"));
    assert!(
        page.contains(
            "Project tags include any of <a href=\"/tag/ci/\">CI</a>, <span \
             class=\"handle-restricted\">Restricted Project</span>"
        ),
        "{page}"
    );
    assert!(!page.contains("Hush"), "{page}");
    assert!(page.contains("Take these actions every time this rule matches:"));
    assert!(page.contains("Add projects: <a href=\"/tag/ci/\">CI</a>."));
    assert!(page.contains("Add subscribers: <a href=\"/p/bob/\">bob</a>."));
    // Only administrators manage global rules.
    assert!(
        page.contains(
            "<span class=\"hm-disabled\" aria-disabled=\"true\"><span class=\"fa fa-pencil\" \
             aria-hidden=\"true\"></span> Edit Rule</span>"
        ),
        "{page}"
    );
    // The viewer comes first among subscribers.
    let alice = page
        .find("<li><a href=\"/p/alice/\">alice</a></li>")
        .unwrap();
    let bob = page.find("<li><a href=\"/p/bob/\">bob</a></li>").unwrap();
    assert!(alice < bob, "{page}");
    assert!(page.contains("created this object."), "{page}");
    assert!(page.contains("renamed this rule from &quot;Tag&quot; to &quot;Tag CI&quot;."));
    assert!(!page.contains("from &quot;&quot;"), "{page}");
    assert!(page.contains("edited this rule."), "{page}");
    assert!(page.contains("/transactions/detail/PHID-XACT-HRUL-4/"));
    assert!(page.contains("disabled this rule."), "{page}");
    let page_root = self::page(test_db, "/H1", ROOT).await;
    assert!(
        page_root.contains("<a href=\"/herald/edit/1/\">"),
        "{page_root}"
    );
    assert!(page_root.contains("/herald/disable/1/disable/"));
    let root = page_root
        .find("<li><a href=\"/p/bob/\">bob</a></li>")
        .unwrap();
    let alice = page_root
        .find("<li><a href=\"/p/alice/\">alice</a></li>")
        .unwrap();
    assert!(alice < root, "{page_root}");

    // A personal rule: its author edits it.
    let page = self::page(test_db, "/H2", BOB).await;
    assert!(
        page.contains("<dt>Rule Type</dt><dd>Personal</dd>"),
        "{page}"
    );
    assert!(
        page.contains("<dt>Author</dt><dd><a href=\"/p/alice/\">alice</a></dd>"),
        "{page}"
    );
    assert!(page.contains("When any of these conditions are met:"));
    assert!(page.contains("Subscribers exists "), "{page}");
    assert!(page.contains("Take these actions the first time this rule matches:"));
    assert!(page.contains("Add rule author as reviewer."));
    assert!(page.contains("hm-disabled\" aria-disabled=\"true\"><span class=\"fa fa-pencil"));
    assert!(page.contains("<p class=\"muted\">None</p>"), "{page}");
    let page = self::page(test_db, "/H2", ALICE).await;
    assert!(page.contains("<a href=\"/herald/edit/2/\">"), "{page}");

    let page = self::page(test_db, "/H3", ALICE).await;
    assert!(page.contains("Author Not Active"), "{page}");
    assert!(page.contains("Take these actions if this rule did not match the last time:"));
    assert!(page.contains("Always  </li>"), "{page}");
    assert!(page.contains("Send an email to rule author."));

    // A transaction, an adapter, a field and object rules not ported.
    assert_eq!(
        fallback(test_db, "/H4", ALICE).await,
        "a Herald rule transaction not ported"
    );
    assert_eq!(
        fallback(test_db, "/H5", ALICE).await,
        "a Herald rule of an adapter not ported"
    );
    assert_eq!(
        fallback(test_db, "/H6", ALICE).await,
        "a Herald field not ported"
    );
    assert_eq!(
        fallback(test_db, "/H8", ALICE).await,
        "an object Herald rule"
    );
    assert_eq!(
        fallback(test_db, "/H7", ALICE).await,
        "an object Herald rule"
    );
    assert_eq!(status(test_db, "/H99", ALICE).await, StatusCode::NOT_FOUND);
    assert_eq!(status(test_db, "/H01", ALICE).await, StatusCode::NOT_FOUND);
}

async fn lists(test_db: &TestDatabase) {
    // Alice's default query: the rules she wrote.
    let page = page(test_db, "/herald/", ALICE).await;
    assert!(page.contains("<a href=\"/H2\">Alice reviews</a>"), "{page}");
    assert!(!page.contains("<a href=\"/H1\">"), "{page}");

    let page = self::page(test_db, "/herald/query/all/", ALICE).await;
    for (id, name) in [
        (1, "Tag CI"),
        (2, "Alice reviews"),
        (3, "Ghost rule"),
        (4, "Off"),
        (5, "Commits"),
        (6, "Custom"),
        (8, "CI object"),
    ] {
        assert!(
            page.contains(&format!("<a href=\"/H{id}\">{name}</a>")),
            "{id} {page}"
        );
    }
    // The object rule of a project Alice can't see is hidden.
    assert!(!page.contains("Hushed object"), "{page}");
    assert!(page.contains("Object Rule"), "{page}");
    assert!(page.contains("Author Not Active"), "{page}");
    assert!(page.contains("Disabled"), "{page}");
    assert!(page.contains("Affects: Commits"), "{page}");
    assert!(
        page.contains("Authored by <a href=\"/p/gone/\" class=\"remarkup-object-closed\">gone</a>"),
        "{page}"
    );
    let h8 = page.find("/H8\"").unwrap();
    let h1 = page.find("/H1\"").unwrap();
    assert!(h8 < h1, "newest first: {page}");

    let page = self::page(test_db, "/herald/query/active/", ALICE).await;
    assert!(page.contains("<a href=\"/H1\">Tag CI</a>"), "{page}");
    assert!(!page.contains("Ghost rule"), "{page}");
    assert!(!page.contains("<a href=\"/H4\">"), "{page}");

    let page = self::page(test_db, "/herald/query/all/?after=3", ALICE).await;
    assert!(page.contains("<a href=\"/H2\">"), "{page}");
    assert!(!page.contains("<a href=\"/H3\">"), "{page}");

    // Bob saved a query, which may be his default.
    assert_eq!(
        fallback(test_db, "/herald/", BOB).await,
        "saved search queries"
    );
    assert_eq!(
        fallback(test_db, "/herald/query/abc/", BOB).await,
        "a saved search query"
    );
    assert_eq!(
        fallback(test_db, "/herald/query/all/?authorPHIDs[0]=x", BOB).await,
        "a search query"
    );

    // Transcripts of objects the viewer can't see are hidden.
    let page = self::page(test_db, "/herald/transcript/", ALICE).await;
    assert!(page.contains("/herald/transcript/1/"), "{page}");
    assert!(page.contains("/herald/transcript/3/"), "{page}");
    assert!(!page.contains("/herald/transcript/2/"), "{page}");
    assert!(
        page.contains("<a href=\"/D1\">D1: Open</a> · 12 ms"),
        "{page}"
    );
    assert!(page.contains("Dry Run · <a href=\"/D1\">D1: Open</a> · 1,500 ms"));
    assert_eq!(
        fallback(test_db, "/herald/transcript/query/abc/", ALICE).await,
        "a saved search query"
    );
}

async fn transcripts(test_db: &TestDatabase) {
    let page = page(test_db, "/herald/transcript/1/", ALICE).await;
    assert!(page.contains("Herald Transcript 1</h1>"), "{page}");
    assert!(page.contains("<a href=\"/H1\">Tag CI</a>"), "{page}");
    assert!(page.contains(
        "Revision title contains <span class=\"condition-test-value\">zz&lt;b&gt;</span>"
    ));
    // Restricted objects show their PHID, as in PHP.
    assert!(
        page.contains(
            "Project tags include any of <span class=\"condition-test-value\">CI, \
             PHID-PROJ-hush</span>"
        ),
        "{page}"
    );
    assert!(page.contains(
        "Revision status is any of <span class=\"condition-test-value\">accepted, draft</span>"
    ));
    assert!(page.contains("<strong>Matched</strong>"), "{page}");
    assert!(page.contains("All conditions matched."));
    assert!(page.contains("Any condition matched."));
    assert!(
        page.contains("<strong>Action: Add projects</strong>"),
        "{page}"
    );
    assert!(page.contains("Added Projects"));
    assert!(page.contains("Added a project: <a href=\"/tag/ci/\">CI</a>."));
    assert!(page.contains("Added a subscriber: <a href=\"/p/bob/\">bob</a>."));
    assert!(page.contains("<strong>Action: Add me as a reviewer</strong>"));
    assert!(page.contains("Revision Author"));
    assert!(
        page.contains(
            "Declined to add revision author as reviewer: <a href=\"/p/alice/\">alice</a>."
        )
    );
    assert!(page.contains("/herald/transcript/1/xactions/"), "{page}");

    let page = self::page(test_db, "/herald/transcript/1/fields/", ALICE).await;
    assert!(page.contains("<dt>Object Name</dt><dd>Open</dd>"), "{page}");
    assert!(page.contains("<dt>Object Link</dt><dd><a href=\"/D1\">D1: Open</a></dd>"));
    assert!(page.contains("<dt>Run At Epoch</dt><dd>November 14th, 10:13:20 PM</dd>"));
    assert!(page.contains("<dt>Run Duration</dt><dd>12 ms</dd>"));
    assert!(page.contains("<dt>Field: Revision title</dt><dd>zz&lt;b&gt; thing</dd>"));
    assert!(
        page.contains("<dt>Field: Revision status</dt><dd>Accepted</dd>"),
        "{page}"
    );
    assert!(
        page.contains(
            "<dt>Field: Project tags</dt><dd><a href=\"/tag/ci/\">CI</a><br /><span \
             class=\"handle-restricted\">Restricted Project</span></dd>"
        ),
        "{page}"
    );

    let page = self::page(test_db, "/herald/transcript/1/profile/", ALICE).await;
    assert!(page.contains("<h2>Profile</h2>"), "{page}");
    assert!(page.contains("<a href=\"/H1\">H1 Tag CI</a>"), "{page}");
    assert!(page.contains("<td>Field</td><td class=\"hm-pri\">Revision title</td>"));

    assert_eq!(
        fallback(test_db, "/herald/transcript/1/xactions/", ALICE).await,
        "the transactions of a transcript"
    );
    assert_eq!(
        status(test_db, "/herald/transcript/1/bogus/", ALICE).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(test_db, "/herald/transcript/2/", ALICE).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(test_db, "/herald/transcript/9/", ALICE).await,
        StatusCode::NOT_FOUND
    );
    let page = self::page(test_db, "/herald/transcript/3/", BOB).await;
    assert!(page.contains("Old Transcript"), "{page}");
    assert!(page.contains("Dry Run"), "{page}");
    assert_eq!(
        fallback(test_db, "/herald/transcript/4/", ALICE).await,
        "a transcript of an adapter not ported"
    );
}

#[tokio::test]
async fn herald_pages_show_what_the_viewer_may_see() {
    let Some(test_db) = setup().await else {
        return;
    };
    rules(&test_db).await;
    lists(&test_db).await;
    transcripts(&test_db).await;
    test_db.drop_databases().await;
}
