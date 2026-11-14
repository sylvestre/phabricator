//! `feed.query_id` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-cccccccccccccccccccccccccccc";
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
const STORY: &str = "PhabricatorApplicationTransactionFeedStory";

fn chronological_key(id: u64) -> u64 {
    (EPOCH << 32) + id
}

async fn query(context: &Context, params: &str) -> Response {
    let mut registry = Registry::default();
    phab_app_feed::register(&mut registry);
    let http = HttpRequest {
        body: format!("api.token={TOKEN}&{params}"),
        ..HttpRequest::default()
    };
    call::dispatch(
        context,
        &registry,
        "feed.query_id",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, params: &str) -> Envelope {
    match query(context, params).await {
        Response::Envelope(envelope) => envelope,
        Response::Fallback(reason) => panic!("unexpected fallback for {params}: {reason}"),
    }
}

async fn ids(context: &Context, params: &str) -> (Vec<String>, Value) {
    let envelope = answer(context, params).await;
    assert_eq!(envelope.error_code, None, "{envelope:?}");
    let ids = envelope.result["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|story| story["id"].as_str().unwrap().to_owned())
        .collect();
    (ids, envelope.result["cursor"].clone())
}

/// A story about `revision` whose transactions are `transactions`, with a
/// reference row unless it is a notification.
async fn add_story(
    test_db: &TestDatabase,
    id: u64,
    story_type: &str,
    revision: &str,
    transactions: &[&str],
    referenced: bool,
) {
    let phids: serde_json::Map<String, Value> = transactions
        .iter()
        .map(|phid| ((*phid).to_owned(), Value::from(*phid)))
        .collect();
    let data = json!({
        "objectPHID": revision,
        "transactionPHIDs": phids,
        "extra": {},
    })
    .to_string();
    let key = chronological_key(id);
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-STRY-{id}', {key}, '{story_type}', '{data}', 'PHID-USER-alice', 1, 1)"
        ))
        .await;
    if referenced {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
                 VALUES ('{revision}', {key}), ('PHID-USER-alice', {key})"
            ))
            .await;
    }
}

async fn add_transaction(test_db: &TestDatabase, phid: &str, revision: &str, kind: &str) {
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_transaction (phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('{phid}', 'PHID-USER-alice', '{revision}', 'public', 'PHID-USER-alice', NULL, 0, \
               '{kind}', 'null', 'true', '{{}}', '{{}}', 1, 1)"
        ))
        .await;
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 1, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (phid, name, versionControlSystem, details, \
               dateCreated, dateModified, viewPolicy, editPolicy, pushPolicy) VALUES \
             ('PHID-REPO-open', 'open', 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin'), \
             ('PHID-REPO-secret', 'secret', 'hg', '{{}}', 1, 1, 'admin', 'admin', 'admin'); \
             INSERT INTO `{{ns}}_differential`.differential_revision (title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             ('a', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', 'k', \
               'users', 'users', 'PHID-REPO-open', '{{}}', ''), \
             ('b', 'PHID-DREV-secret', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', 'k', \
               'users', 'users', 'PHID-REPO-secret', '{{}}', '')"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

/// Stories 1, 5 and 7 are visible to bob.
async fn add_stories(test_db: &TestDatabase) {
    let transactions = [
        (
            "PHID-XACT-DREV-1",
            "PHID-DREV-open",
            "differential.revision.accept",
        ),
        (
            "PHID-XACT-DREV-2",
            "PHID-DREV-secret",
            "differential.revision.accept",
        ),
        ("PHID-XACT-DREV-3", "PHID-DREV-open", "token:give"),
        (
            "PHID-XACT-DREV-5",
            "PHID-DREV-open",
            "differential.revision.close",
        ),
        (
            "PHID-XACT-DREV-7",
            "PHID-DREV-open",
            "differential.revision.abandon",
        ),
    ];
    for (phid, revision, kind) in transactions {
        add_transaction(test_db, phid, revision, kind).await;
    }
    let stories: [(u64, &str, &str, &[&str], bool); 7] = [
        (1, STORY, "PHID-DREV-open", &["PHID-XACT-DREV-1"], true),
        // The revision's repository is hidden from bob.
        (2, STORY, "PHID-DREV-secret", &["PHID-XACT-DREV-2"], true),
        // Hidden from feed.
        (3, STORY, "PHID-DREV-open", &["PHID-XACT-DREV-3"], true),
        // A class which doesn't exist.
        (
            4,
            "PhabricatorFeedStoryGone",
            "PHID-DREV-open",
            &["PHID-XACT-DREV-1"],
            true,
        ),
        // One transaction is enough to show the story.
        (
            5,
            STORY,
            "PHID-DREV-open",
            &["PHID-XACT-DREV-3", "PHID-XACT-DREV-5"],
            true,
        ),
        // A notification, without references.
        (6, STORY, "PHID-DREV-open", &["PHID-XACT-DREV-1"], false),
        (7, STORY, "PHID-DREV-open", &["PHID-XACT-DREV-7"], true),
    ];
    for (id, story_type, revision, transactions, referenced) in stories {
        add_story(test_db, id, story_type, revision, transactions, referenced).await;
    }
}

#[tokio::test]
async fn pages_through_visible_stories() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_stories(&test_db).await;

    let envelope = answer(&context, "limit=2").await;
    assert_eq!(
        envelope.result,
        json!({
            "data": [
                {
                    "id": "1",
                    "phid": "PHID-STRY-1",
                    "class": STORY,
                    "epoch": EPOCH,
                    "authorPHID": "PHID-USER-alice",
                    "chronologicalKey": chronological_key(1).to_string(),
                    "data": {
                        "objectPHID": "PHID-DREV-open",
                        "transactionPHIDs": {"PHID-XACT-DREV-1": "PHID-XACT-DREV-1"},
                        "extra": [],
                    },
                },
                envelope.result["data"][1].clone(),
            ],
            "cursor": {"limit": "2", "after": "5", "before": null},
        })
    );
    assert_eq!(envelope.result["data"][1]["id"], "5");

    assert_eq!(
        ids(&context, "limit=2&after=5").await,
        (
            vec!["7".to_owned()],
            json!({"limit": "2", "after": null, "before": "7"})
        )
    );
    // max(1, "1") returns the integer.
    assert_eq!(
        ids(&context, "limit=1&before=7").await,
        (
            vec!["5".to_owned()],
            json!({"limit": 1, "after": "5", "before": "5"})
        )
    );
    assert_eq!(
        ids(&context, "before=5").await,
        (
            vec!["1".to_owned()],
            json!({"limit": 100, "after": "1", "before": null})
        )
    );
    // Strictly typed parameters keep their types.
    assert_eq!(
        ids(
            &context,
            &format!(
                "params=%7B%22limit%22%3A10%2C%22after%22%3A1%2C%22__conduit__%22%3A\
                 %7B%22token%22%3A%22{TOKEN}%22%7D%7D"
            )
        )
        .await,
        (
            vec!["5".to_owned(), "7".to_owned()],
            json!({"limit": 10, "after": null, "before": "5"})
        )
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn rejects_cursors_and_views_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_stories(&test_db).await;

    for cursor in ["2", "3", "4", "6", "0", "99"] {
        assert_eq!(
            answer(&context, &format!("after={cursor}")).await,
            Envelope::error(
                "ERR-CONDUIT-CORE",
                format!(
                    "Cursor \"{cursor}\" does not identify a valid object in query \
                     \"PhabricatorFeedIDQuery\"."
                )
            )
        );
    }
    assert_eq!(
        answer(&context, "view=bogus").await,
        Envelope::error(
            "ERR-UNKNOWN-TYPE",
            "Unsupported view type, possibles are: html, data, html-summary, text"
        )
    );
    // The view is only checked for the stories returned.
    assert_eq!(
        ids(&context, "after=7&view=bogus").await,
        (vec![], json!({"limit": 100, "after": null, "before": null}))
    );
    assert!(matches!(
        query(&context, "view=text").await,
        Response::Fallback(_)
    ));
    assert!(matches!(
        query(&context, "after=1x").await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_unported_stories_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_stories(&test_db).await;

    // Custom fields enabled by differential.fields need PHP.
    add_transaction(
        &test_db,
        "PHID-XACT-DREV-8",
        "PHID-DREV-open",
        "core:customfield",
    )
    .await;
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_transaction \
             SET metadata = '{\"customfield:key\":\"phabricator:revert-plan\"}' \
             WHERE phid = 'PHID-XACT-DREV-8'",
        )
        .await;
    add_story(
        &test_db,
        8,
        STORY,
        "PHID-DREV-open",
        &["PHID-XACT-DREV-8"],
        true,
    )
    .await;
    assert!(matches!(
        query(&context, "after=7").await,
        Response::Fallback(_)
    ));
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_transaction \
             SET transactionType = 'differential.revision.accept' WHERE phid = 'PHID-XACT-DREV-8'",
        )
        .await;
    assert_eq!(ids(&context, "after=7").await.0, ["8"]);
    // So do stories about other objects.
    add_story(
        &test_db,
        9,
        STORY,
        "PHID-TASK-1",
        &["PHID-XACT-TASK-1"],
        true,
    )
    .await;
    assert!(matches!(
        query(&context, "after=7").await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn overheats_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    add_transaction(
        &test_db,
        "PHID-XACT-DREV-1",
        "PHID-DREV-secret",
        "differential.revision.accept",
    )
    .await;
    for id in 1..=25 {
        add_story(
            &test_db,
            id,
            STORY,
            "PHID-DREV-secret",
            &["PHID-XACT-DREV-1"],
            true,
        )
        .await;
    }
    assert_eq!(
        answer(&context, "limit=1").await,
        Envelope::error(
            "ERR-CONDUIT-CORE",
            "Query (of class \"PhabricatorFeedIDQuery\") overheated: examined more than 20 raw \
             rows without finding 2 visible objects."
        )
    );
    // Fewer rows than the overheat limit is a short, empty page.
    assert_eq!(
        ids(&context, "limit=3").await,
        (vec![], json!({"limit": "3", "after": null, "before": null}))
    );
    // A huge limit has no overheat limit, as PHP's overflows to a float.
    assert_eq!(
        ids(&context, "limit=9223372036854775807").await,
        (
            vec![],
            json!({"limit": "9223372036854775807", "after": null, "before": null})
        )
    );

    test_db.drop_databases().await;
}
