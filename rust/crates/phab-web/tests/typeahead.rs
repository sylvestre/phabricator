//! The typeahead datasources (`/typeahead/class/<class>/`) against one
//! test database: what PHP's tokenizers receive, byte for byte.
//!
//! Users: albert (disabled), alfred (a bot), alice, and bob, whose real
//! name is Bob Alvarez. Projects: Alpha (#alpha) and Beta Alpha (#beta),
//! with the builtin image of their icon.

// The checks share one database, so they are one long function.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::json;
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

const ALICE: &str = "phsid=alice-token";

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

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user", "auth", "project", "policy", "spaces", "file", "calendar",
    ])
    .await?;
    let briefcase =
        phab_app_project::typeahead::builtin_file_key("projects/v3/briefcase.png").unwrap();
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (1, 'PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             (2, 'PHID-USER-albert', 'albert', 'Albert', 1, 1, 'c', 0, 1, 0, 1, 1, 's', 0, 0), \
             (3, 'PHID-USER-alfred', 'alfred', '', 1, 1, 'c', 1, 0, 0, 1, 1, 's', 0, 0), \
             (4, 'PHID-USER-bob', 'bob', 'Bob Alvarez', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_user`.user_nametoken (token, userID) VALUES \
             ('alice', 1), ('albert', 2), ('alfred', 3), ('bob', 4), ('alvarez', 4); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0); \
             INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             (1, 'Alpha', 'PHID-PROJ-alpha', 'x', 1, 1, '0', 'users', 'users', 'users', \
               'project', 'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{{}}', 'default', 'alpha'), \
             (2, 'Beta Alpha', 'PHID-PROJ-beta', 'x', 1, 1, '100', 'users', 'users', 'users', \
               'project', 'red', 'k', 0, 0, 0, 'b', 0, 'k002', '{{}}', 'default', 'beta'); \
             INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, \
               dateModified) VALUES \
             ('PHID-PROJ-alpha', 'alpha', 1, 1), ('PHID-PROJ-beta', 'beta', 1, 1); \
             INSERT INTO `{{ns}}_project`.project_datasourcetoken (projectID, token) VALUES \
             (1, 'alpha'), (2, 'beta'), (2, 'alpha'); \
             INSERT INTO `{{ns}}_file`.file (id, phid, name, mimeType, byteSize, storageEngine, \
               storageFormat, storageHandle, dateCreated, dateModified, secretKey, metadata, \
               mailKey, viewPolicy, builtinKey) VALUES \
             (1, 'PHID-FILE-brief', 'briefcase.png', 'image/png', 1, 'blob', 'raw', '1', 1, 1, \
               'keybxxxxxxxxxxxxxxxx', '{{\"builtin\":\"{briefcase}\",\"canCDN\":true}}', 'k', \
               'public', '{briefcase}')",
            alice = session_key("alice-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

async fn post(test_db: &TestDatabase, class: &str, body: &str) -> Response {
    let mut config = Config::default();
    if let serde_json::Value::Object(map) = json!({"phabricator.base-uri": "http://phab.test/"}) {
        config.push_layer("test", map);
    }
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let request = Request::post(format!("/typeahead/class/{class}/"))
        .header(header::COOKIE, ALICE)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap()
}

/// The payload of an Ajax response, after its shield.
async fn payload(test_db: &TestDatabase, class: &str, body: &str) -> String {
    let response = post(test_db, class, body).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    assert_eq!(status, StatusCode::OK, "{class} {body}: {reason:?}");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=UTF-8"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        response.headers()[header::EXPIRES],
        "Sat, 01 Jan 2000 00:00:00 GMT"
    );
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.strip_prefix("for (;;);{\"error\":null,\"payload\":")
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or_else(|| panic!("not an Ajax response: {text}"))
        .to_owned()
}

async fn fallback(test_db: &TestDatabase, class: &str, body: &str) -> String {
    let response = post(test_db, class, body).await;
    assert_eq!(
        response.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "{class} {body}"
    );
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn typeahead() {
    let Some(test_db) = setup().await else {
        return;
    };
    let image = "http:\\/\\/phab.test\\/file\\/data\\/keybxxxxxxxxxxxxxxxx\\/PHID-FILE-brief\\/\
                 briefcase.png";

    // Users starting with "al", then Bob through his real name.
    assert_eq!(
        payload(
            &test_db,
            "PhabricatorPeopleDatasource",
            "q=al&raw=al&__ajax__=true"
        )
        .await,
        "[[\"albert (Albert)\",\"\\/p\\/albert\\/\",\"PHID-USER-albert\",\"albert\",null,null,\
         null,\"user\",\"fa-user bluegrey\",\"Disabled\",null,null,null,null,\"@albert\",\
         \"prefix\"],\
         [\"alfred\",\"\\/p\\/alfred\\/\",\"PHID-USER-alfred\",\"alfred\",null,null,null,\
         \"user\",\"fa-user bluegrey\",\"Bot\",null,null,null,null,\"@alfred\",\"prefix\"],\
         [\"alice (Alice)\",\"\\/p\\/alice\\/\",\"PHID-USER-alice\",\"alice\",null,null,null,\
         \"user\",\"fa-user bluegrey\",null,null,null,null,null,\"@alice\",\"prefix\"],\
         [\"bob (Bob Alvarez)\",\"\\/p\\/bob\\/\",\"PHID-USER-bob\",\"bob\",null,null,null,\
         \"user\",\"fa-user bluegrey\",null,null,null,null,null,\"@bob\",\"content\"]]"
    );
    // Nothing typed: every user, by name.
    let everyone = payload(&test_db, "PhabricatorPeopleDatasource", "__ajax__=1").await;
    assert!(everyone.starts_with("[[\"albert (Albert)\""), "{everyone}");
    assert!(everyone.contains("\"@bob\",\"content\"]]"), "{everyone}");

    // Projects: "#alp" matches names in the content phase only.
    assert_eq!(
        payload(
            &test_db,
            "PhabricatorProjectDatasource",
            "q=alp&raw=alp&__ajax__=true"
        )
        .await,
        format!(
            "[[\"Alpha\\nalpha\",\"\\/project\\/view\\/1\\/\",\"PHID-PROJ-alpha\",null,\"Alpha\",\
             \"Project\",\"{image}\",\"proj\",\"fa-briefcase\",null,null,\"blue\",null,null,\
             \"#alpha\",\"prefix\"],\
             [\"Beta Alpha\\nbeta\",\"\\/project\\/view\\/2\\/\",\"PHID-PROJ-beta\",null,\
             \"Beta Alpha\",\"Project\",\"{image}\",\"proj\",\"fa-briefcase\",\"Archived\",null,\
             \"red\",null,null,\"#beta\",\"content\"]]"
        )
    );
    let hashtag = payload(
        &test_db,
        "PhabricatorProjectDatasource",
        "q=%23alp&raw=%23alp&__ajax__=true",
    )
    .await;
    assert!(
        hashtag.starts_with("[[\"Alpha\\nalpha\"") && hashtag.contains("\"#alpha\",\"content\"]"),
        "{hashtag}"
    );

    // Reviewers: users, projects and their blocking functions, by name.
    let reviewers = payload(
        &test_db,
        "DifferentialReviewerDatasource",
        "q=alice&raw=alice&__ajax__=true",
    )
    .await;
    assert_eq!(
        reviewers,
        "[[\"alice (Alice)\",\"\\/p\\/alice\\/\",\"PHID-USER-alice\",\"alice\",null,null,null,\
         \"user\",\"fa-user bluegrey\",null,null,null,null,null,\"@alice\",\"prefix\"],\
         [\"alice (Alice) blocking\",\"\\/p\\/alice\\/\",\"blocking(PHID-USER-alice)\",\"alice\",\
         \"Blocking: alice (Alice)\",null,null,\"user\",\"fa-asterisk\",null,null,\"red\",\
         \"function\",null,\"@alice\",\"prefix\"]]"
    );

    // What PHP keeps.
    assert_eq!(
        fallback(&test_db, "PhabricatorPeopleDatasource", "q=al").await,
        "the typeahead debug page"
    );
    assert_eq!(
        fallback(
            &test_db,
            "DifferentialReviewerDatasource",
            "raw=blocking(al&__ajax__=1"
        )
        .await,
        "a typeahead function"
    );
    assert_eq!(
        fallback(&test_db, "PhabricatorSearchDatasource", "__ajax__=1").await,
        "a datasource not ported"
    );
    assert_eq!(
        fallback(
            &test_db,
            "PhabricatorPeopleDatasource",
            "select=%7B%7D&__ajax__=1"
        )
        .await,
        "a typeahead selection"
    );
    test_db
        .execute(
            "UPDATE `{ns}_user`.user SET availabilityCache = '{\"until\":1700003600}', \
             availabilityCacheTTL = 1700003600 WHERE id = 1",
        )
        .await;
    assert_eq!(
        fallback(
            &test_db,
            "PhabricatorPeopleDatasource",
            "raw=alice&__ajax__=1"
        )
        .await,
        "a user who is away"
    );
    test_db.drop_databases().await;
}
