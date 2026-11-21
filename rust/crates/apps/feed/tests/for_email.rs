//! `feed.for_email.*` against a test database. The expected answers follow
//! moz-extensions `src/email/` (`EmailEventBuilder` and its models).

// The fixtures and the expected answers are long.
#![allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::{Config, hash, php};
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "api-cccccccccccccccccccccccccccc";
const OTHER_TOKEN: &str = "api-dddddddddddddddddddddddddddd";
const DATABASES: &[&str] = &[
    "feed",
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
];
const EPOCH: u64 = 1_700_000_000;
const NOW: i64 = 1_700_000_000;
const STORY: &str = "PhabricatorApplicationTransactionFeedStory";

fn key(id: u64) -> u64 {
    (EPOCH << 32) + id
}

/// A Bugzilla answering the bug summary requests of `BugStore`.
async fn bugzilla() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split(' ').nth(1).unwrap_or("");
                let (status, body) =
                    if path == "/rest/bug/123?include_fields=summary&api_key=secret" {
                        (
                            "200 OK",
                            r#"{"bugs":[{"summary":"Crash in éditeur / parser"}]}"#,
                        )
                    } else {
                        ("404 Not Found", r#"{"error":true}"#)
                    };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}")
}

async fn call(context: &Context, method: &str, token: &str, params: &str) -> Response {
    let mut registry = Registry::default();
    phab_app_feed::register(&mut registry);
    let http = HttpRequest {
        body: format!("api.token={token}&{params}"),
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, method, &http, Mode::Shadow, NOW)
        .await
        .response
}

async fn answer(context: &Context, method: &str, params: &str) -> Envelope {
    match call(context, method, TOKEN, params).await {
        Response::Envelope(envelope) => envelope,
        Response::Fallback(reason) => panic!("unexpected fallback for {method} {params}: {reason}"),
    }
}

/// The JSON string a query method returns, decoded.
async fn query(context: &Context, method: &str, params: &str) -> Value {
    let envelope = answer(context, method, params).await;
    assert_eq!(envelope.error_code, None, "{envelope:?}");
    let text = envelope
        .result
        .as_str()
        .expect("the methods return a string");
    serde_json::from_str(text).unwrap()
}

async fn fallback(context: &Context, method: &str, params: &str) -> &'static str {
    match call(context, method, TOKEN, params).await {
        Response::Fallback(reason) => reason,
        Response::Envelope(envelope) => panic!("expected a fallback, got {envelope:?}"),
    }
}

fn user_rows() -> String {
    [
        (1, "bob", "Bob", 0),
        (2, "alice", "Alice", 0),
        (3, "carol", "Carol C.", 0),
        (4, "dave", "Dave", 0),
        (5, "phab-bot", "Bot", 0),
        (6, "erin", "Erin", 1),
        (7, "frank", "Frank", 0),
    ]
    .iter()
    .map(|(id, name, real, disabled)| {
        format!(
            "({id}, 'PHID-USER-{name}', '{name}', '{real}', 1, 1, 'c', 0, {disabled}, 0, 1, 1, 's', 0, 0)"
        )
    })
    .collect::<Vec<_>>()
    .join(", ")
}

fn project_row(id: u32, name: &str, phid: &str) -> String {
    format!(
        "({id}, '{name}', '{phid}', 'PHID-USER-bob', 1, 1, '0', 'users', 'users', 'users', 'project', \
         'blue', 'k', 0, 0, 0, 'p{id}', 0, 'k{id:03}', '{{}}', 'default', NULL, NULL, NULL)"
    )
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    let bug_index = hash::digest_for_index(b"differential:bugzilla-bug-id");
    let users = user_rows();
    let projects = [
        project_row(1, "mozilla-phabricator-emails", "PHID-PROJ-emails"),
        project_row(2, "secure-revision", "PHID-PROJ-secure"),
        project_row(3, "reviewers-group", "PHID-PROJ-group"),
    ]
    .join(", ");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES {users}; \
             INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, isPrimary, \
               dateCreated, dateModified) VALUES \
             ('PHID-EADR-1', 'PHID-USER-bob', 'bob@test.com', 1, 1, 1, 1), \
             ('PHID-EADR-2', 'PHID-USER-alice', 'alice@test.com', 1, 1, 1, 1), \
             ('PHID-EADR-3', 'PHID-USER-alice', 'alice@old.test.com', 1, 0, 1, 1), \
             ('PHID-EADR-4', 'PHID-USER-carol', 'carol@test.com', 1, 1, 1, 1), \
             ('PHID-EADR-5', 'PHID-USER-dave', 'dave@test.com', 1, 1, 1, 1), \
             ('PHID-EADR-6', 'PHID-USER-erin', 'erin@test.com', 1, 1, 1, 1), \
             ('PHID-EADR-7', 'PHID-USER-frank', 'frank@test.com', 1, 1, 1, 1); \
             INSERT INTO `{{ns}}_user`.user_preferences (phid, userPHID, preferences, dateCreated, \
               dateModified, builtinKey) VALUES \
             ('PHID-PSET-1', 'PHID-USER-alice', '{{\"no-mail\":\"2\",\"timezone\":\"Europe/Paris\"}}', 1, 1, NULL), \
             ('PHID-PSET-2', 'PHID-USER-carol', '{{\"no-mail\":\"2\"}}', 1, 1, NULL), \
             ('PHID-PSET-3', 'PHID-USER-erin', '{{\"no-mail\":\"2\"}}', 1, 1, NULL), \
             ('PHID-PSET-4', 'PHID-USER-dave', '{{\"timezone\":\"America/New_York\"}}', 1, 1, NULL), \
             ('PHID-PSET-5', NULL, '{{\"no-mail\":\"1\"}}', 1, 1, 'global'); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1), \
             ('PHID-USER-frank', 'api', '{OTHER_TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, dateModified, \
               status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, \
               hasMilestones, hasSubprojects, projectPath, projectDepth, projectPathKey, properties, \
               subtype, parentProjectPHID, milestoneNumber, primarySlug) VALUES {projects}; \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PROJ-emails', 60, 'PHID-USER-bob', 1, 0), \
             ('PHID-PROJ-group', 60, 'PHID-USER-alice', 1, 0), \
             ('PHID-PROJ-group', 60, 'PHID-USER-carol', 1, 0), \
             ('PHID-PROJ-group', 47, 'PHID-USER-dave', 1, 0); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, versionControlSystem, details, \
               dateCreated, dateModified, viewPolicy, editPolicy, pushPolicy) VALUES \
             (1, 'PHID-REPO-git', 'mozilla-central', 'git', '{{}}', 1, 1, 'users', 'admin', 'admin'), \
             (2, 'PHID-REPO-hg', 'try', 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{{ns}}_repository`.repository_uri (phid, repositoryPHID, uri, ioType, \
               displayType, isDisabled, dateCreated, dateModified) VALUES \
             ('PHID-RURI-1', 'PHID-REPO-hg', 'https://hg.test/old-try', 'none', 'always', 0, 1, 1), \
             ('PHID-RURI-2', 'PHID-REPO-hg', 'https://hg.test/try', 'observe', 'always', 0, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, summary, importStatus) VALUES \
             (1, 1, 'PHID-CMIT-git', 'abcdef0123', 1, 'none', 's', 15), \
             (2, 2, 'PHID-CMIT-hg', '0123abcdef', 1, 'none', 's', 15); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Bug 123 - Fix the\\nparser', 'PHID-DREV-1', 'needs-review', '', '', \
               'PHID-USER-alice', 1, 1, '{{}}', 'k', 'users', 'users', 'PHID-REPO-git', '{{}}', \
               'PHID-DIFF-2'), \
             (2, 'Secret fix', 'PHID-DREV-2', 'accepted', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'no-one', 'users', 'PHID-REPO-git', '{{}}', 'PHID-DIFF-2'), \
             (3, 'Draft', 'PHID-DREV-3', 'draft', '', '', 'PHID-USER-alice', 1, 1, '{{}}', 'k', \
               'users', 'users', 'PHID-REPO-git', '{{\"draft.broadcast\":false}}', ''), \
             (4, 'Secret without bug', 'PHID-DREV-4', 'needs-review', '', '', 'PHID-USER-alice', 1, \
               1, '{{}}', 'k', 'users', 'users', 'PHID-REPO-git', '{{}}', ''), \
             (5, 'Unknown bug', 'PHID-DREV-5', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', 'PHID-REPO-hg', '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-carol', 'added', 1, 1, '{{}}'), \
             ('PHID-DREV-1', 'PHID-USER-dave', 'resigned', 1, 1, '{{}}'), \
             ('PHID-DREV-2', 'PHID-PROJ-group', 'added', 1, 1, '{{}}'), \
             ('PHID-DREV-5', 'PHID-USER-carol', 'accepted', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_customfieldstorage (objectPHID, \
               fieldIndex, fieldValue) VALUES ('PHID-DREV-1', '{bug_index}', '123'), \
               ('PHID-DREV-2', '{bug_index}', '456'), ('PHID-DREV-5', '{bug_index}', '999'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-dave', 2, 0), \
             ('PHID-DREV-1', 21, 'PHID-USER-erin', 1, 0), \
             ('PHID-DREV-1', 21, 'PHID-USER-alice', 1, 1), \
             ('PHID-DREV-2', 41, 'PHID-PROJ-secure', 1, 0), \
             ('PHID-DREV-4', 41, 'PHID-PROJ-secure', 1, 0), \
             ('PHID-DREV-1', 41, 'PHID-PROJ-group', 1, 0)"
        ))
        .await;
    let mut config = Config::default();
    let Value::Object(values) = json!({
        "phabricator.base-uri": "http://phab.test/",
        "bugzilla.url": bugzilla().await,
        "bugzilla.automation_api_key": "secret",
        "lando.url": "https://lando.test",
    }) else {
        unreachable!()
    };
    config.push_layer("test", values);
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

/// A transaction of `author` on `revision`.
async fn add_transaction(
    test_db: &TestDatabase,
    id: u64,
    revision: &str,
    author: &str,
    kind: &str,
    old: &str,
    new: &str,
    metadata: &str,
    comment: Option<&str>,
) {
    let comment_phid = match comment {
        Some(content) => {
            test_db
                .execute(&format!(
                    "INSERT INTO `{{ns}}_differential`.differential_transaction_comment (id, phid, \
                       transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
                       contentSource, isDeleted, dateCreated, dateModified, revisionPHID, \
                       changesetID, isNewFile, lineNumber, lineLength, hasReplies, attributes) \
                     VALUES ({id}, 'PHID-XCMT-{id}', 'PHID-XACT-DREV-{id}', '{author}', 'users', \
                       '{author}', 1, '{content}', '{{}}', 0, 1, 1, '{revision}', NULL, 0, 0, 0, 0, \
                       '{{}}')"
                ))
                .await;
            format!("'PHID-XCMT-{id}'")
        }
        None => "NULL".to_owned(),
    };
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_transaction (id, phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-XACT-DREV-{id}', '{author}', '{revision}', 'public', '{author}', \
               {comment_phid}, 0, '{kind}', '{old}', '{new}', '{{}}', '{metadata}', 1, 1)"
        ))
        .await;
}

/// A story of `author` about `object` with these transactions.
async fn add_story(
    test_db: &TestDatabase,
    id: u64,
    object: &str,
    author: &str,
    transactions: &[u64],
) {
    let phids: Vec<String> = transactions
        .iter()
        .map(|id| format!("PHID-XACT-DREV-{id}"))
        .collect();
    let data = json!({"objectPHID": object, "transactionPHIDs": phids}).to_string();
    let key = key(id);
    let created = NOW + i64::try_from(id).unwrap();
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-STRY-{id}', {key}, '{STORY}', '{data}', '{author}', {created}, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('{object}', {key})"
        ))
        .await;
}

const ALICE: &str = "PHID-USER-alice";
const CAROL: &str = "PHID-USER-carol";

/// Stories 1 to 7; 3 is about a draft and 7 is hidden from feed.
async fn add_stories(test_db: &TestDatabase) {
    // 1: carol accepts D1 with a comment.
    add_transaction(
        test_db,
        11,
        "PHID-DREV-1",
        CAROL,
        "differential.revision.accept",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_transaction(
        test_db,
        12,
        "PHID-DREV-1",
        CAROL,
        "core:comment",
        "null",
        "null",
        "{}",
        Some("Looks **good**"),
    )
    .await;
    add_story(test_db, 1, "PHID-DREV-1", CAROL, &[11, 12]).await;
    // 2: carol comments on the secure D2, pinging alice twice.
    add_transaction(
        test_db,
        21,
        "PHID-DREV-2",
        CAROL,
        "core:comment",
        "null",
        "null",
        "{}",
        Some("@alice please check, @alice. Also @nobody"),
    )
    .await;
    add_story(test_db, 2, "PHID-DREV-2", CAROL, &[21]).await;
    // 3: the draft D3 is abandoned.
    add_transaction(
        test_db,
        31,
        "PHID-DREV-3",
        ALICE,
        "differential.revision.abandon",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_story(test_db, 3, "PHID-DREV-3", ALICE, &[31]).await;
    // 4: alice abandons D1, with a transaction of Herald's.
    add_transaction(
        test_db,
        40,
        "PHID-DREV-1",
        "PHID-APPS-PhabricatorHeraldApplication",
        "differential.revision.abandon",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_transaction(
        test_db,
        41,
        "PHID-DREV-1",
        ALICE,
        "differential.revision.abandon",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_story(test_db, 4, "PHID-DREV-1", ALICE, &[41, 40]).await;
    // 5: the secure D4 has no bug, which PHP can't put in its context.
    add_transaction(
        test_db,
        51,
        "PHID-DREV-4",
        ALICE,
        "differential.revision.abandon",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_story(test_db, 5, "PHID-DREV-4", ALICE, &[51]).await;
    // 6: D1 lands.
    add_transaction(
        test_db,
        61,
        "PHID-DREV-1",
        ALICE,
        "differential.revision.close",
        "null",
        "true",
        r#"{"isCommitClose":true,"commitPHID":"PHID-CMIT-git"}"#,
        None,
    )
    .await;
    add_story(test_db, 6, "PHID-DREV-1", ALICE, &[61]).await;
    // 7: hidden from feed.
    add_transaction(
        test_db,
        71,
        "PHID-DREV-1",
        ALICE,
        "token:give",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_story(test_db, 7, "PHID-DREV-1", ALICE, &[71]).await;
}

fn recipient(name: &str, offset: i64, is_actor: bool) -> Value {
    json!({
        "email": format!("{name}@test.com"),
        "username": name,
        "timezoneOffset": offset,
        "isActor": is_actor,
    })
}

fn minimal(revision: u64, recipients: Value) -> Value {
    json!({
        "revision": {"revisionId": revision, "link": format!("http://phab.test/D{revision}")},
        "recipients": recipients,
    })
}

fn event(id: u64, secure: bool, minimal: Value, context: Value) -> Value {
    json!({
        "key": key(id).to_string(),
        "timestamp": NOW + i64::try_from(id).unwrap(),
        "isSecure": secure,
        "minimalContext": minimal,
        "context": context,
    })
}

fn with_feed_id(mut event: Value, id: u64) -> Value {
    event["feedId"] = json!(id);
    event
}

fn email_context(kind: &str, actor: (&str, &str), revision: Value, body: Value) -> Value {
    json!({
        "eventKind": kind,
        "actorName": actor.0,
        "actor": {"userName": actor.0, "realName": actor.1},
        "revision": revision,
        "body": body,
    })
}

fn d1(bugzilla: &str) -> Value {
    json!({
        "revisionId": 1,
        "name": "Bug 123 - Fix the parser",
        "link": "http://phab.test/D1",
        "repositoryName": "mozilla-central",
        "bug": {
            "bugId": 123,
            "name": "Crash in \u{e9}diteur / parser",
            "link": format!("{bugzilla}/show_bug.cgi?id=123"),
        },
    })
}

fn carol_reviewer(is_actor: bool) -> Value {
    json!({
        "name": "carol",
        "isActionable": false,
        "status": "unreviewed",
        "recipients": [recipient("carol", 0, is_actor)],
    })
}

/// The events of stories 1 to 6.
fn expected_events(bugzilla: &str) -> Vec<Value> {
    let d2 = json!({
        "revisionId": 2,
        "link": "http://phab.test/D2",
        "bug": {"bugId": 456, "link": format!("{bugzilla}/show_bug.cgi?id=456")},
    });
    let alice = recipient("alice", 3600, false);
    let group = json!({
        "name": "reviewers-group",
        "isActionable": false,
        "status": "unreviewed",
        // Members come newest first.
        "recipients": [recipient("carol", 0, true), alice],
    });
    vec![
        event(
            1,
            false,
            minimal(1, json!([alice, recipient("carol", 0, true), alice])),
            email_context(
                "revision-accepted",
                ("carol", "Carol C."),
                d1(bugzilla),
                json!({
                    "mainCommentMessage": {"asText": "Looks **good**\n", "asHtml": "<p>Looks <strong>good</strong></p>"},
                    "inlineComments": [],
                    "transactionLink": "http://phab.test/D1#11",
                    "landoLink": "https://lando.test/D1/",
                    "isReadyToLand": false,
                    "subscribers": [alice],
                    "reviewers": [carol_reviewer(true)],
                    "author": alice,
                }),
            ),
        ),
        event(
            2,
            true,
            minimal(2, json!([recipient("carol", 0, true), alice, alice])),
            email_context(
                "revision-commented",
                ("carol", "Carol C."),
                d2.clone(),
                json!({
                    "subscribers": [],
                    "reviewers": [group],
                    "author": alice,
                    "transactionLink": "http://phab.test/D2#21",
                }),
            ),
        ),
        event(
            2,
            true,
            minimal(2, json!([recipient("carol", 0, true), alice, alice])),
            email_context(
                "revision-comment-pinged",
                ("carol", "Carol C."),
                d2,
                json!({"recipient": alice, "transactionLink": "http://phab.test/D2#21"}),
            ),
        ),
        event(
            4,
            false,
            minimal(
                1,
                json!([
                    recipient("alice", 3600, true),
                    recipient("carol", 0, false),
                    recipient("alice", 3600, true)
                ]),
            ),
            email_context(
                "revision-abandoned",
                ("alice", "Alice"),
                d1(bugzilla),
                json!({
                    "mainCommentMessage": null,
                    "inlineComments": [],
                    "transactionLink": "http://phab.test/D1#41",
                    "reviewers": [carol_reviewer(false)],
                    "subscribers": [recipient("alice", 3600, true)],
                }),
            ),
        ),
        // PHP sends a minimal event, not secure, when building fails.
        event(
            5,
            false,
            minimal(4, json!([recipient("alice", 3600, true)])),
            Value::Null,
        ),
        event(
            6,
            false,
            minimal(
                1,
                json!([
                    recipient("alice", 3600, true),
                    recipient("carol", 0, false),
                    recipient("alice", 3600, true)
                ]),
            ),
            email_context(
                "revision-landed",
                ("alice", "Alice"),
                d1(bugzilla),
                json!({
                    "subscribers": [recipient("alice", 3600, true)],
                    "reviewers": [carol_reviewer(false)],
                    "author": recipient("alice", 3600, true),
                    "revisionHash": "abcdef0123",
                    "hgLink": null,
                    "landoLink": "https://lando.test/D1/",
                }),
            ),
        ),
    ]
}

fn bugzilla_of(context: &Context) -> String {
    context.config.string("bugzilla.url").unwrap().to_owned()
}

#[tokio::test]
async fn builds_the_events_of_each_story() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_stories(&test_db).await;
    let expected = expected_events(&bugzilla_of(&context));

    let envelope = answer(&context, "feed.for_email.query_id", "").await;
    let ids = [1, 2, 2, 4, 5, 6];
    let events: Vec<Value> = expected
        .iter()
        .zip(ids)
        .map(|(event, id)| with_feed_id(event.clone(), id))
        .collect();
    // The exact string PHP's json_encode() gives.
    assert_eq!(
        envelope.result,
        Value::String(php::json_encode(&json!({
            "data": {"events": events, "storyErrors": 0},
            "cursor": {"limit": 100, "after": 7},
        })))
    );
    let text = envelope.result.as_str().unwrap();
    assert!(text.contains(r"Crash in \u00e9diteur \/ parser"), "{text}");

    assert_eq!(
        query(&context, "feed.for_email.query", "").await,
        json!({
            "data": {"events": expected, "storyErrors": 0},
            "cursor": {"limit": 100, "after": key(6).to_string()},
        })
    );
}

#[tokio::test]
async fn pages_by_story_id() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_stories(&test_db).await;
    let expected = expected_events(&bugzilla_of(&context));
    let method = "feed.for_email.query_id";

    let page = query(&context, method, "storyLimit=2").await;
    assert_eq!(page["data"]["events"].as_array().unwrap().len(), 3);
    assert_eq!(page["cursor"], json!({"limit": 2, "after": 2}));

    // The draft's story counts in the page but has no event.
    let page = query(&context, method, "storyLimit=2&after=2").await;
    assert_eq!(
        page["data"]["events"],
        json!([with_feed_id(expected[3].clone(), 4)])
    );
    assert_eq!(page["cursor"], json!({"limit": 2, "after": 4}));

    // The cursor moves past stories which are filtered out.
    let page = query(&context, method, "storyLimit=5&after=6").await;
    assert_eq!(
        page,
        json!({"data": {"events": [], "storyErrors": 0}, "cursor": {"limit": 5, "after": 7}})
    );
    let page = query(&context, method, "after=7").await;
    assert_eq!(page["cursor"], json!({"limit": 100, "after": 7}));

    // Limits are clamped to 1..1000.
    let page = query(&context, method, "storyLimit=0").await;
    assert_eq!(page["cursor"], json!({"limit": 1, "after": 1}));
    let page = query(&context, method, "storyLimit=5000&after=5").await;
    assert_eq!(page["cursor"], json!({"limit": 1000, "after": 7}));
}

#[tokio::test]
async fn pages_by_chronological_key() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_stories(&test_db).await;
    let expected = expected_events(&bugzilla_of(&context));
    let method = "feed.for_email.query";

    let page = query(&context, method, "storyLimit=2").await;
    assert_eq!(page["data"]["events"], json!(expected[..3]));
    assert_eq!(
        page["cursor"],
        json!({"limit": 2, "after": key(2).to_string()})
    );

    let page = query(&context, method, &format!("storyLimit=2&after={}", key(2))).await;
    assert_eq!(page["data"]["events"], json!([expected[3]]));
    assert_eq!(
        page["cursor"],
        json!({"limit": 2, "after": key(4).to_string()})
    );

    // No more stories: the cursor stays.
    let page = query(&context, method, &format!("after={}", key(6))).await;
    assert_eq!(
        page,
        json!({"data": {"events": [], "storyErrors": 0}, "cursor": {"limit": 100, "after": key(6).to_string()}})
    );
    // A page size of at least one, but the limit as given.
    let page = query(&context, method, "storyLimit=-4").await;
    assert_eq!(
        page["cursor"],
        json!({"limit": -4, "after": key(1).to_string()})
    );

    // The cursor must be a story the query returns.
    let envelope = answer(&context, method, &format!("after={}", key(7))).await;
    assert_eq!(envelope.error_code.as_deref(), Some("ERR-CONDUIT-CORE"));
    assert_eq!(
        envelope.error_info.as_deref(),
        Some(
            format!(
                "Cursor \"{}\" does not identify a valid object in query \"PhabricatorFeedQuery\".",
                key(7)
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn reports_the_newest_story() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    assert_eq!(
        answer(&context, "feed.for_email.status_id", "")
            .await
            .result,
        json!(0)
    );
    assert!(
        fallback(&context, "feed.for_email.status", "")
            .await
            .contains("empty")
    );

    add_stories(&test_db).await;
    // The newest story is hidden: status skips it, status_id doesn't.
    assert_eq!(
        answer(&context, "feed.for_email.status_id", "")
            .await
            .result,
        json!(7)
    );
    assert_eq!(
        answer(&context, "feed.for_email.status", "").await.result,
        json!(key(6).to_string())
    );
}

#[tokio::test]
async fn requires_the_email_group() {
    let Some((_test_db, context)) = setup().await else {
        return;
    };
    for method in [
        "feed.for_email.query",
        "feed.for_email.query_id",
        "feed.for_email.status",
        "feed.for_email.status_id",
    ] {
        let Response::Envelope(envelope) = call(&context, method, OTHER_TOKEN, "").await else {
            panic!("unexpected fallback");
        };
        assert_eq!(envelope.error_code.as_deref(), Some("ERR-INVALID-AUTH"));
        assert_eq!(
            envelope.error_info.as_deref(),
            Some("Must be part of the \"mozilla-phabricator-emails\" security group")
        );
    }
}

#[tokio::test]
async fn builds_updates_reviews_and_metadata_edits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let bugzilla = bugzilla_of(&context);
    // Diffs 1 and 2 differ in one line; diff 3 only in context.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, lintStatus, \
               unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 0, 0, 1, 1, 1, 'users'), \
             (2, 'PHID-DIFF-2', 1, 0, 0, 1, 1, 1, 'users'), \
             (3, 'PHID-DIFF-3', 1, 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{ns}_differential`.differential_changeset (id, phid, diffID, filename, \
               changeType, fileType, metadata, oldProperties, newProperties, addLines, delLines, \
               dateCreated, dateModified) VALUES \
             (1, 'PHID-DCHG-1', 1, 'a.txt', 2, 1, '{}', '{}', '{}', 1, 1, 1, 1), \
             (2, 'PHID-DCHG-2', 2, 'a.txt', 2, 1, '{}', '{}', '{}', 1, 1, 1, 1), \
             (3, 'PHID-DCHG-3', 2, 'new/b.txt', 1, 1, '{\"hash.effect\":\"h1\"}', '{}', '{}', 1, 0, 1, 1), \
             (4, 'PHID-DCHG-4', 3, 'a.txt', 2, 1, '{}', '{}', '{}', 1, 1, 1, 1), \
             (5, 'PHID-DCHG-5', 3, 'new/b.txt', 1, 1, '{\"hash.effect\":\"h1\"}', '{}', '{}', 1, 0, 1, 1); \
             INSERT INTO `{ns}_differential`.differential_hunk (changesetID, oldOffset, oldLen, \
               newOffset, newLen, dataType, dataEncoding, dataFormat, data, dateCreated, \
               dateModified) VALUES \
             (1, 1, 2, 1, 2, 'text', 'utf8', 'byte', ' one\\n-two\\n+three', 1, 1), \
             (2, 1, 2, 1, 2, 'text', 'utf8', 'byte', ' one\\n-two\\n+3', 1, 1), \
             (3, 0, 0, 1, 1, 'text', 'utf8', 'byte', '+b', 1, 1), \
             (4, 1, 2, 1, 2, 'text', 'utf8', 'byte', ' ONE\\n-two\\n+3', 1, 1), \
             (5, 0, 0, 1, 1, 'text', 'utf8', 'byte', '+b', 1, 1)",
        )
        .await;
    // 1: alice updates D1 to diff 2, which changes the code.
    add_transaction(
        &test_db,
        11,
        "PHID-DREV-1",
        ALICE,
        "differential:update",
        r#""PHID-DIFF-1""#,
        r#""PHID-DIFF-2""#,
        "{}",
        None,
    )
    .await;
    add_story(&test_db, 1, "PHID-DREV-1", ALICE, &[11]).await;
    // 2: then to diff 3, which only changes context: no event.
    add_transaction(
        &test_db,
        21,
        "PHID-DREV-1",
        ALICE,
        "differential:update",
        r#""PHID-DIFF-2""#,
        r#""PHID-DIFF-3""#,
        "{}",
        None,
    )
    .await;
    add_story(&test_db, 2, "PHID-DREV-1", ALICE, &[21]).await;
    // 3: alice adds carol as reviewer of D5 and changes its title.
    add_transaction(
        &test_db,
        31,
        "PHID-DREV-5",
        ALICE,
        "differential.revision.reviewers",
        "{}",
        r#"{"PHID-USER-carol":"accepted"}"#,
        "{}",
        None,
    )
    .await;
    add_transaction(
        &test_db,
        32,
        "PHID-DREV-5",
        ALICE,
        "differential.revision.title",
        r#""a""#,
        r#""b""#,
        "{}",
        None,
    )
    .await;
    add_story(&test_db, 3, "PHID-DREV-5", ALICE, &[31, 32]).await;
    // 4: alice requests changes on D5, which bmo doesn't know the bug of.
    add_transaction(
        &test_db,
        41,
        "PHID-DREV-5",
        ALICE,
        "differential.revision.reject",
        "null",
        "true",
        "{}",
        None,
    )
    .await;
    add_story(&test_db, 4, "PHID-DREV-5", ALICE, &[41]).await;

    let alice = recipient("alice", 3600, true);
    let carol = recipient("carol", 0, false);
    let d5 = json!({
        "revisionId": 5,
        "name": "Unknown bug",
        "link": "http://phab.test/D5",
        "repositoryName": "try",
        "bug": {
            "bugId": 999,
            "name": "(failed to fetch bug name)",
            "link": format!("{bugzilla}/show_bug.cgi?id=999"),
        },
    });
    let page = query(&context, "feed.for_email.query", "").await;
    assert_eq!(
        page["data"]["events"],
        json!([
            event(
                1,
                false,
                minimal(1, json!([alice, carol, alice])),
                email_context(
                    "revision-updated",
                    ("alice", "Alice"),
                    d1(&bugzilla),
                    json!({
                        // Changesets come newest first.
                        "affectedFiles": [
                            {"path": "/new/b.txt", "change": "added"},
                            {"path": "/a.txt", "change": "modified"},
                        ],
                        "landoLink": "https://lando.test/D1/",
                        "newChangesLink": "http://phab.test/D1?vs=1&id=2",
                        "isReadyToLand": false,
                        "reviewers": [{
                            "name": "carol",
                            "isActionable": true,
                            "status": "unreviewed",
                            "recipients": [carol],
                        }],
                        "subscribers": [alice],
                    }),
                ),
            ),
            event(
                3,
                false,
                minimal(5, json!([carol, alice])),
                email_context(
                    "revision-metadata-edited",
                    ("alice", "Alice"),
                    d5.clone(),
                    json!({
                        "isReadyToLand": false,
                        "isTitleChanged": true,
                        "isBugChanged": false,
                        "author": alice,
                        "reviewers": [{
                            "name": "carol",
                            "isActionable": false,
                            "status": "accepted",
                            "metadataChange": "added",
                            "recipients": [carol],
                        }],
                        "subscribers": [],
                    }),
                ),
            ),
            event(
                4,
                false,
                minimal(5, json!([carol, alice])),
                email_context(
                    "revision-requested-changes",
                    ("alice", "Alice"),
                    d5,
                    json!({
                        "transactionLink": "http://phab.test/D5#41",
                        "mainCommentMessage": null,
                        "inlineComments": [],
                        "subscribers": [],
                        "reviewers": [{
                            "name": "carol",
                            "isActionable": false,
                            "status": "accepted",
                            "recipients": [carol],
                        }],
                        "author": alice,
                    }),
                ),
            ),
        ])
    );
}

#[tokio::test]
async fn leaves_what_is_not_ported_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // A public comment mentioning a user: its rendering needs the mention.
    add_transaction(
        &test_db,
        11,
        "PHID-DREV-1",
        CAROL,
        "core:comment",
        "null",
        "null",
        "{}",
        Some("Thanks @alice"),
    )
    .await;
    add_story(&test_db, 1, "PHID-DREV-1", CAROL, &[11]).await;
    assert!(
        fallback(&context, "feed.for_email.query_id", "")
            .await
            .contains("referencing")
    );

    // A story about another kind of object.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             (2, 'PHID-STRY-2', {key}, '{STORY}', \
               '{{\"objectPHID\":\"PHID-TASK-1\",\"transactionPHIDs\":[\"PHID-XACT-TASK-1\"]}}', \
               'PHID-USER-alice', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('PHID-TASK-1', {key})",
            key = key(2)
        ))
        .await;
    assert!(
        fallback(&context, "feed.for_email.query_id", "after=1")
            .await
            .contains("other than revisions")
    );
    // Unusual parameters.
    assert!(
        fallback(&context, "feed.for_email.query_id", "storyLimit=abc")
            .await
            .contains("parameter")
    );
}
