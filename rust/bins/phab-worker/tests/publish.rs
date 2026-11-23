//! `PhabricatorApplicationTransactionPublishWorker` for Differential
//! revisions against a throwaway `MySQL` database: the rows it writes,
//! compared to what PHP's `publishTransactions()` writes for the same task
//! (derived from the PHP code, see each test), and the tasks it leaves to
//! PHP untouched.
//!
//! The task data is what `PhabricatorApplicationTransactionEditor` queues:
//! the object, the acting user, the transaction PHIDs and the editor's
//! `getWorkerState()`, as `DifferentialTransactionEditor` builds it.

#![allow(clippy::too_many_lines)]

use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, rows};
use phab_worker::handlers::publish::PublishWorker;
use phab_worker::{Executed, LeaseOwner, Registry, Worker};
use serde_json::{Map, Value, json};

const CLASS: &str = "PhabricatorApplicationTransactionPublishWorker";
const DATABASES: &[&str] = &[
    "worker",
    "differential",
    "user",
    "project",
    "metamta",
    "feed",
    "herald",
    "repository",
    "policy",
    "spaces",
];
const BASE_URI: &str = "http://phab.example.com/";
const HERALD: &str = "PHID-APPS-PhabricatorHeraldApplication";

/// The style of the "View Revision" button of the HTML body.
const BUTTON: &str = "text-decoration: none; padding: 4px 8px; margin: 0 8px 8px; float: right; \
     color: #464C5C; font-weight: bold; border-radius: 3px; background-color: #F7F7F9; \
     background-image: linear-gradient(to bottom,#fff,#f1f0f1); display: inline-block; \
     border: 1px solid rgba(71,87,120,.2);";

type Row = Map<String, Value>;

struct Fixture {
    test_db: TestDatabase,
}

/// Users alice (who wrote D1), bob (its reviewer), carol, dave (who
/// subscribed) and erin, each with the settings cache PHP builds on login.
/// D1 is in rVCT, its active diff on the branch `feature`; D2 is alice's
/// draft. `#secure-revision` is the project of Mozilla's secure revisions.
async fn fixture() -> Option<Fixture> {
    let test_db = TestDatabase::create(DATABASES).await?;
    let index = phab_core::hash::digest_for_index(b"user.preferences.v2");
    for name in ["alice", "bob", "carol", "dave", "erin"] {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, \
                   dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
                   isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
                   isMailingList) VALUES \
                 ('PHID-USER-{name}', '{name}', 'Real {name}', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, \
                   0); \
                 INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, \
                   isPrimary, dateCreated, dateModified) VALUES \
                 ('PHID-EADR-{name}', 'PHID-USER-{name}', '{name}@example.com', 1, 1, 1, 1); \
                 INSERT INTO `{{ns}}_user`.user_cache (userPHID, cacheIndex, cacheKey, \
                   cacheData, cacheType) VALUES \
                 ('PHID-USER-{name}', '{index}', 'user.preferences.v2', '[]', 'preferences')"
            ))
            .await;
    }
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-vct', 'Version Control Tools', 'VCT', 'git', '{}', 1, 1, 'users', \
               'admin', 'admin'); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Fix the thing', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, \
               1, '{}', 'k', 'users', 'users', 'PHID-REPO-vct', '{}', 'PHID-DIFF-1'), \
             (2, 'Draft', 'PHID-DREV-2', 'draft', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', NULL, '{}', 'PHID-DIFF-2'); \
             INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, sourceControlBaseRevision, branch, bookmark, lintStatus, unitStatus, \
               lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', NULL, 'feature', NULL, 0, 0, 1, 1, 1, \
               'users'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', NULL, NULL, NULL, 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{}'); \
             INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-dave', 1, 0); \
             INSERT INTO `{ns}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug, parentProjectPHID, \
               milestoneNumber) VALUES \
             (1, 'secure-revision', 'PHID-PROJ-secure', 'x', 1, 1, '0', 'users', 'users', \
               'users', 'fa-lock', 'red', 'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', \
               'secure-revision', NULL, NULL)",
        )
        .await;
    Some(Fixture { test_db })
}

/// The configuration: the base URI and `extra`.
fn config(extra: &Value) -> Config {
    let mut layer = Map::new();
    layer.insert("phabricator.base-uri".to_owned(), json!(BASE_URI));
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        layer.insert(key, value);
    }
    let mut config = Config::default();
    config.push_layer("test", layer);
    config
}

/// A mail stamp of the worker state (`PhabricatorMailStamp::toDictionary()`).
fn stamp(kind: &str, key: &str, value: Value) -> Value {
    let mut stamp = json!({"type": kind, "key": key});
    stamp["value"] = value;
    stamp
}

/// The stamps `newMailStamps()` stores for an edit of D1 by `actor`, with
/// `extra` replacing some values.
fn stamps(actor: &str, extra: &[(&str, Value)]) -> Value {
    let mut list = vec![
        stamp("phid", "author", json!("PHID-USER-alice")),
        stamp("phid", "reviewer", json!(["PHID-USER-bob"])),
        stamp("phid", "blocking-reviewer", json!([])),
        stamp("phid", "resigned-reviewer", json!([])),
        stamp("phid", "revision-repository", json!("PHID-REPO-vct")),
        stamp("phid", "revision-status", json!("needs-review")),
        stamp("string", "application", json!("Differential")),
        stamp("string", "monogram", json!("D1")),
        stamp("string", "phid", json!("PHID-DREV-1")),
        stamp("string", "object-type", json!("DREV")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!("conduit")),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!([])),
        stamp("bool", "new", json!(false)),
        stamp("phid", "mention", json!([])),
        stamp("string", "herald", json!([])),
        stamp("phid", "removed", json!([])),
        stamp("phid", "tag", json!([])),
        stamp("phid", "subscriber", json!(["PHID-USER-dave"])),
    ];
    for (key, value) in extra {
        for item in &mut list {
            if item["key"] == *key {
                item["value"] = value.clone();
            }
        }
    }
    Value::Array(list)
}

/// `getWorkerState()` of an edit of D1 by `actor`, as
/// `DifferentialTransactionEditor` leaves it, with `extra` replacing some
/// properties.
fn state(actor: &str, extra: &Value) -> Value {
    let mut state = json!({
        "parentMessageID": null,
        "isNewObject": false,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": null,
        "mailToPHIDs": ["PHID-USER-alice", "PHID-USER-bob"],
        "mailCCPHIDs": ["PHID-USER-dave"],
        "feedNotifyPHIDs": ["PHID-USER-alice", "PHID-USER-bob", "PHID-USER-dave"],
        "feedRelatedPHIDs": ["PHID-DREV-1", actor],
        "feedShouldPublish": true,
        "mailShouldSend": true,
        "mustEncrypt": [],
        "mailStamps": stamps(actor, &[]),
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": [],
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": {
            "changedPriorToCommitURI": null,
            "firstBroadcast": false,
            "isDraftDemotion": null,
        },
        "custom.encoding": [],
    });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        state[key] = value;
    }
    state
}

/// The task data of a publish task.
fn data(object: &str, actor: &str, xactions: &[&str], state: Value) -> Value {
    let mut data = json!({
        "objectPHID": object,
        "actorPHID": actor,
        "xactionPHIDs": xactions,
    });
    data["state"] = state;
    data
}

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    /// A transaction of `object`, as the editor saved it.
    async fn transaction(
        &self,
        (phid, author, object): (&str, &str, &str),
        kind: &str,
        (old, new): (&Value, &Value),
        (metadata, comment): (&Value, Option<&str>),
    ) {
        let comment_phid = comment.map(|_| format!("PHID-XCMT-{phid}"));
        let sql = format!(
            "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, \
               commentVersion, transactionType, oldValue, newValue, contentSource, metadata, \
               dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, ?, ?, ?, ?, ?, \
               '{{\"source\":\"conduit\",\"params\":[]}}', ?, 1, 1)",
            self.database()
                .table("differential", "differential_transaction")
        );
        sqlx::query(&sql)
            .bind(phid)
            .bind(author)
            .bind(object)
            .bind(author)
            .bind(&comment_phid)
            .bind(i64::from(comment.is_some()))
            .bind(kind)
            .bind(phab_core::php::json_encode(old))
            .bind(phab_core::php::json_encode(new))
            .bind(phab_core::php::json_encode(metadata))
            .execute(self.database().pool())
            .await
            .unwrap();
        if let (Some(comment), Some(comment_phid)) = (comment, comment_phid) {
            let sql = format!(
                "INSERT INTO {} (phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
                   commentVersion, content, contentSource, isDeleted, dateCreated, \
                   dateModified, revisionPHID, changesetID, isNewFile, lineNumber, lineLength, \
                   fixedState, hasReplies, replyToCommentPHID, attributes) VALUES \
                 (?, ?, ?, 'public', ?, 1, ?, '{{}}', 0, 1, 1, ?, NULL, 0, 0, 0, NULL, 0, NULL, \
                   '{{}}')",
                self.database()
                    .table("differential", "differential_transaction_comment")
            );
            sqlx::query(&sql)
                .bind(&comment_phid)
                .bind(phid)
                .bind(author)
                .bind(author)
                .bind(comment)
                .bind(object)
                .execute(self.database().pool())
                .await
                .unwrap();
        }
    }

    async fn schedule(&self, data: &Value) -> i64 {
        let data = phab_core::php::json_encode(data);
        let mut connection = self.database().pool().acquire().await.unwrap();
        let id = schedule_task(
            &mut connection,
            self.database(),
            Task {
                class: CLASS,
                data: &data,
                priority: 1000,
                object_phid: Some("PHID-DREV-1"),
            },
            1_600_000_000,
        )
        .await
        .unwrap();
        i64::try_from(id).unwrap()
    }

    fn worker(&self, config: Config) -> Worker {
        let mut registry = Registry::default();
        registry.register(PublishWorker);
        Worker::new(
            self.database().clone(),
            config,
            registry,
            LeaseOwner::new("rust-test"),
        )
    }

    /// Queues the task and runs it.
    async fn run(&self, config: Config, data: &Value) -> (i64, Executed) {
        let task = self.schedule(data).await;
        let ran = self
            .worker(config)
            .work_on(None)
            .await
            .unwrap()
            .expect("a task to run");
        assert_eq!(ran.id, task);
        (task, ran.outcome.unwrap())
    }

    /// Every row of a query, each column as text.
    async fn rows(&self, sql: &str) -> Vec<Row> {
        let sql = sql.replace("{ns}", self.database().namespace());
        let found = sqlx::query(&sql)
            .fetch_all(self.database().pool())
            .await
            .unwrap();
        found
            .iter()
            .map(|row| {
                use sqlx::{Column, Row as _};
                row.columns()
                    .iter()
                    .map(|column| {
                        let value = rows::opt_text(row, column.name())
                            .unwrap()
                            .map_or(Value::Null, Value::String);
                        (column.name().to_owned(), value)
                    })
                    .collect()
            })
            .collect()
    }

    /// The tasks queued besides `except`: class, priority, object, data.
    async fn queued(&self, except: i64) -> Vec<(String, String, Option<String>, String)> {
        self.rows(&format!(
            "SELECT CAST(task.id AS CHAR) id, CAST(taskClass AS CHAR) taskClass, \
               CAST(priority AS CHAR) priority, CAST(objectPHID AS CHAR) objectPHID, \
               CAST(data.data AS CHAR) data FROM `{{ns}}_worker`.worker_activetask task \
             JOIN `{{ns}}_worker`.worker_taskdata data ON data.id = task.dataID \
             WHERE task.id != {except} ORDER BY task.id"
        ))
        .await
        .into_iter()
        .map(|row| {
            (
                text(&row, "taskClass").to_owned(),
                text(&row, "priority").to_owned(),
                row["objectPHID"].as_str().map(str::to_owned),
                text(&row, "data").to_owned(),
            )
        })
        .collect()
    }

    /// The mails, oldest first, their parameters decoded in order.
    async fn mails(&self) -> Vec<Row> {
        self.rows(
            "SELECT CAST(id AS CHAR) id, CAST(phid AS CHAR) phid, \
               CAST(actorPHID AS CHAR) actorPHID, CAST(parameters AS CHAR) parameters, \
               CAST(status AS CHAR) status, CAST(message AS CHAR) message, \
               CAST(relatedPHID AS CHAR) relatedPHID FROM `{ns}_metamta`.metamta_mail ORDER BY id",
        )
        .await
    }

    /// The recipient edges of the mails: source, type, destination, seq.
    async fn mail_edges(&self) -> Vec<(String, String, String, String)> {
        self.rows(
            "SELECT CAST(src AS CHAR) src, CAST(type AS CHAR) type, CAST(dst AS CHAR) dst, \
               CAST(seq AS CHAR) seq FROM `{ns}_metamta`.edge ORDER BY src, dst",
        )
        .await
        .into_iter()
        .map(|row| {
            (
                text(&row, "src").to_owned(),
                text(&row, "type").to_owned(),
                text(&row, "dst").to_owned(),
                text(&row, "seq").to_owned(),
            )
        })
        .collect()
    }

    /// The feed story rows: data, references and notifications.
    async fn story(&self) -> Option<(Row, Vec<String>, Vec<(String, String)>)> {
        let mut stories = self
            .rows(
                "SELECT CAST(chronologicalKey AS CHAR) chronologicalKey, \
                   CAST(phid AS CHAR) phid, CAST(storyType AS CHAR) storyType, \
                   CAST(storyData AS CHAR) storyData, CAST(authorPHID AS CHAR) authorPHID \
                 FROM `{ns}_feed`.feed_storydata",
            )
            .await;
        assert!(stories.len() <= 1, "{stories:?}");
        let story = stories.pop()?;
        let key = text(&story, "chronologicalKey").to_owned();
        let references = self
            .rows(&format!(
                "SELECT CAST(objectPHID AS CHAR) objectPHID FROM `{{ns}}_feed`.feed_storyreference \
                 WHERE chronologicalKey = {key} ORDER BY objectPHID"
            ))
            .await
            .iter()
            .map(|row| text(row, "objectPHID").to_owned())
            .collect();
        let notifications = self
            .rows(&format!(
                "SELECT CAST(userPHID AS CHAR) userPHID, CAST(hasViewed AS CHAR) hasViewed, \
                   CAST(primaryObjectPHID AS CHAR) primaryObjectPHID \
                 FROM `{{ns}}_feed`.feed_storynotification WHERE chronologicalKey = {key} \
                 ORDER BY userPHID"
            ))
            .await
            .iter()
            .map(|row| {
                assert_eq!(text(row, "primaryObjectPHID"), "PHID-DREV-1");
                (
                    text(row, "userPHID").to_owned(),
                    text(row, "hasViewed").to_owned(),
                )
            })
            .collect();
        Some((story, references, notifications))
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

fn text<'a>(row: &'a Row, column: &str) -> &'a str {
    row[column]
        .as_str()
        .unwrap_or_else(|| panic!("{column} in {row:?}"))
}

/// The parameters of a mail row, decoded with their order.
fn parameters(row: &Row) -> Map<String, Value> {
    serde_json::from_str(text(row, "parameters")).unwrap()
}

/// The search task `queueDocumentForIndexing()` queues, with
/// `PRIORITY_INDEX` and the object.
fn search_task(object: &str, xactions: &[&str]) -> (String, String, Option<String>, String) {
    let phids: Map<String, Value> = xactions
        .iter()
        .map(|phid| ((*phid).to_owned(), json!(phid)))
        .collect();
    (
        "PhabricatorSearchWorker".to_owned(),
        "3500".to_owned(),
        Some(object.to_owned()),
        phab_core::php::json_encode(&json!({
            "documentPHID": object,
            "parameters": {"transactionPHIDs": phab_core::php::round_trip(&Value::Object(phids))},
            "strict": false,
        })),
    )
}

/// The tasks a publication queues, in order: the search task, the
/// `FeedPublisherWorker` of the story (key `story_key`), then a
/// `PhabricatorMetaMTAWorker` for each mail (`PRIORITY_ALERTS`).
fn expected_tasks(
    search: (String, String, Option<String>, String),
    story_key: Option<&str>,
    mail_ids: &[&str],
) -> Vec<(String, String, Option<String>, String)> {
    let mut tasks = vec![search];
    if let Some(key) = story_key {
        tasks.push((
            "FeedPublisherWorker".to_owned(),
            "2000".to_owned(),
            None,
            format!("{{\"key\":{key}}}"),
        ));
    }
    for id in mail_ids {
        tasks.push((
            "PhabricatorMetaMTAWorker".to_owned(),
            "1000".to_owned(),
            None,
            (*id).to_owned(),
        ));
    }
    tasks
}

/// The HTML of the transaction headers (`addHeadersAndCommentsToMailBody()`).
fn header_table(titles: &str, uri: &str) -> String {
    format!(
        "<table><tr><td style=\"\">{titles}\n</td><a style=\"{BUTTON}\" href=\"{uri}\">View \
         Revision</a></tr></table>"
    )
}

/// The sections after the comments, as `buildMailBody()` and
/// `buildMailForTarget()` of D1 add them: text, then HTML.
fn trailer(branch: bool) -> (String, String) {
    let link = |header: &str, uri: &str| {
        (
            format!("{header}\n  {uri}"),
            format!("<div><strong>{header}</strong><div><a href=\"{uri}\">{uri}</a></div></div>"),
        )
    };
    let mut sections = vec![(
        "REPOSITORY\n  rVCT Version Control Tools".to_owned(),
        "<div><strong>REPOSITORY</strong><div><div>rVCT Version Control Tools</div></div></div>"
            .to_owned(),
    )];
    if branch {
        sections.push((
            "BRANCH\n  feature".to_owned(),
            "<div><strong>BRANCH</strong><div><div>feature</div></div></div>".to_owned(),
        ));
    }
    sections.push(link(
        "CHANGES SINCE LAST ACTION",
        "http://phab.example.com/D1/new/",
    ));
    sections.push(link("REVISION DETAIL", "http://phab.example.com/D1"));
    sections.push(link(
        "EMAIL PREFERENCES",
        "http://phab.example.com/settings/panel/emailpreferences/",
    ));
    let text: Vec<String> = sections.iter().map(|(text, _)| text.clone()).collect();
    let html: Vec<String> = sections.into_iter().map(|(_, html)| html).collect();
    (text.join("\n\n"), html.join("<br />"))
}

/// The rendered stamps of an edit of D1 by bob: `natcasesort()` keeps each
/// stamp's index, in the template order (author 0, reviewer 1, repository
/// 2, status 3, application 4, monogram 5, phid 6, object type 7, actor 8,
/// via 9, subscriber 10).
fn rendered_stamps() -> Value {
    json!({
        "8": "actor(_@bob)",
        "4": "application(Differential)",
        "0": "author(_@alice)",
        "5": "monogram(D1)",
        "7": "object-type(DREV)",
        "6": "phid(PHID-DREV-1)",
        "1": "reviewer(_@bob)",
        "2": "revision-repository(rVCT)",
        "3": "revision-status(needs-review)",
        "10": "subscriber(_@dave)",
        "9": "via(conduit)",
    })
}

/// A comment by bob on D1: one mail per recipient (alice and bob in "To",
/// dave in "Cc"), each with the whole parameter list
/// `buildMailForTarget()` and `willSendMail()` set, in their order; a feed
/// story referenced by D1 and bob, notifying alice and dave (bob wrote it)
/// as read, since they get the mail; the search, feed and mail tasks.
#[tokio::test]
async fn publishes_a_comment_like_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .transaction(
            ("PHID-XACT-DREV-c1", "PHID-USER-bob", "PHID-DREV-1"),
            "core:comment",
            (&Value::Null, &Value::Null),
            (&json!({}), Some("Looks good to me.")),
        )
        .await;
    let data = data(
        "PHID-DREV-1",
        "PHID-USER-bob",
        &["PHID-XACT-DREV-c1"],
        state("PHID-USER-bob", &json!({})),
    );
    let (task, outcome) = fixture.run(config(&json!({})), &data).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    let mails = fixture.mails().await;
    assert_eq!(mails.len(), 3);
    let (trailer_text, trailer_html) = trailer(false);
    let body = format!(
        "bob added a comment.\n\n\n  Looks good to me.\n\n{trailer_text}\n\nTo: alice, bob\nCc: \
         dave\n"
    );
    let html = format!(
        "{}<br /><div><div><p>Looks good to me.</p></div></div><br />{trailer_html}<br />\
         <div><strong>To: </strong>alice, bob<br /><strong>Cc: </strong>dave<br /></div>",
        header_table("bob added a comment.", "http://phab.example.com/D1")
    );
    for (row, (key, recipient)) in mails.iter().zip([
        ("to", "PHID-USER-alice"),
        ("to", "PHID-USER-bob"),
        ("cc", "PHID-USER-dave"),
    ]) {
        assert_eq!(text(row, "actorPHID"), "PHID-USER-bob");
        assert_eq!(text(row, "status"), "queued");
        assert_eq!(row["message"], Value::Null);
        assert_eq!(text(row, "relatedPHID"), "PHID-DREV-1");
        assert!(text(row, "phid").starts_with("PHID-MTAM-"));
        let mut expected = json!({
            "sensitive": false,
            "mustEncrypt": false,
            "subject": "D1: Fix the thing",
            "mustEncrypt.subject": "D1: Revision Updated",
            "mustEncrypt.uri": "/D1",
            "from": "PHID-USER-bob",
            "subject-prefix": "[Differential]",
            "vary-subject-prefix": "[Commented On]",
            "thread-id": "differential-rev-PHID-DREV-1-req",
            "is-first-message": false,
            "exclude": [],
            "muted": [],
            "herald-force-recipients": [],
            "mailtags": ["differential-comment"],
            "is-bulk": true,
            "body": body,
            "html-body": html,
            "stampMetadata": stamps("PHID-USER-bob", &[]),
            "stamps": rendered_stamps(),
            "headers": [
                ["X-Phabricator-To", "<PHID-USER-alice>"],
                ["X-Phabricator-To", "<PHID-USER-bob>"],
                ["X-Phabricator-Cc", "<PHID-USER-dave>"],
            ],
        });
        expected[key] = json!([recipient]);
        assert_eq!(
            Value::Object(parameters(row)),
            phab_core::php::round_trip(&expected)
        );
        // The column is PHP's `json_encode()`.
        assert_eq!(
            text(row, "parameters"),
            phab_core::php::json_encode(&phab_core::php::round_trip(&expected))
        );
    }
    // `PhabricatorMetaMTAMailHasRecipientEdgeType` (57) to the recipient.
    let mut edges: Vec<(String, String, String, String)> = mails
        .iter()
        .zip(["PHID-USER-alice", "PHID-USER-bob", "PHID-USER-dave"])
        .map(|(row, user)| {
            (
                text(row, "phid").to_owned(),
                "57".to_owned(),
                user.to_owned(),
                "0".to_owned(),
            )
        })
        .collect();
    edges.sort();
    assert_eq!(fixture.mail_edges().await, edges);

    let (story, references, notifications) = fixture.story().await.expect("a story");
    assert!(text(&story, "phid").starts_with("PHID-STRY-"));
    assert_eq!(
        text(&story, "storyType"),
        "PhabricatorApplicationTransactionFeedStory"
    );
    assert_eq!(
        text(&story, "storyData"),
        "{\"objectPHID\":\"PHID-DREV-1\",\"transactionPHIDs\":{\"PHID-XACT-DREV-c1\":\
         \"PHID-XACT-DREV-c1\"}}"
    );
    assert_eq!(text(&story, "authorPHID"), "PHID-USER-bob");
    let key = text(&story, "chronologicalKey");
    let epoch = key.parse::<u64>().unwrap() >> 32;
    assert!(epoch > 1_700_000_000, "{key}");
    assert_eq!(references, ["PHID-DREV-1", "PHID-USER-bob"]);
    assert_eq!(
        notifications,
        [
            ("PHID-USER-alice".to_owned(), "1".to_owned()),
            ("PHID-USER-dave".to_owned(), "1".to_owned()),
        ]
    );

    let ids: Vec<&str> = mails.iter().map(|row| text(row, "id")).collect();
    assert_eq!(
        fixture.queued(task).await,
        expected_tasks(
            search_task("PHID-DREV-1", &["PHID-XACT-DREV-c1"]),
            Some(key),
            &ids
        )
    );
    fixture.finish().await;
}

/// With Mozilla's mail by default (`email.default` 2), Phabricator sends
/// none of this mail, but Mozilla's reaches the recipients, so their
/// notifications are read (`filterDeliverableByAnyMailActors()`), as PHP
/// marks them on the dev stack.
#[tokio::test]
async fn marks_notifications_read_for_mozilla_mail() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .transaction(
            ("PHID-XACT-DREV-c1", "PHID-USER-bob", "PHID-DREV-1"),
            "core:comment",
            (&Value::Null, &Value::Null),
            (&json!({}), Some("Looks good to me.")),
        )
        .await;
    let data = data(
        "PHID-DREV-1",
        "PHID-USER-bob",
        &["PHID-XACT-DREV-c1"],
        state("PHID-USER-bob", &json!({})),
    );
    let (_, outcome) = fixture
        .run(config(&json!({"email.default": "2"})), &data)
        .await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");
    assert_eq!(fixture.mails().await.len(), 3);
    let (_, _, notifications) = fixture.story().await.expect("a story");
    assert_eq!(
        notifications,
        [
            ("PHID-USER-alice".to_owned(), "1".to_owned()),
            ("PHID-USER-dave".to_owned(), "1".to_owned()),
        ]
    );
    fixture.finish().await;
}

/// bob accepts D1, which the edit made accepted: the accept, then the
/// status change, both "differential-other". The strongest action (300)
/// names the mail, both titles head it, and an accepted revision shows its
/// branch. The story lists the accept first.
#[tokio::test]
async fn publishes_an_accept_like_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET status = 'accepted' WHERE id = 1",
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-st", "PHID-USER-bob", "PHID-DREV-1"),
            "differential.revision.status",
            (&json!("needs-review"), &json!("accepted")),
            (&json!({}), None),
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-ac", "PHID-USER-bob", "PHID-DREV-1"),
            "differential.revision.accept",
            (&json!(false), &json!(true)),
            (&json!({}), None),
        )
        .await;
    let xactions = ["PHID-XACT-DREV-ac", "PHID-XACT-DREV-st"];
    let state = state(
        "PHID-USER-bob",
        &json!({"mailStamps": stamps("PHID-USER-bob", &[("revision-status", json!("accepted"))])}),
    );
    let data = data("PHID-DREV-1", "PHID-USER-bob", &xactions, state);
    let (task, outcome) = fixture.run(config(&json!({})), &data).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    let mails = fixture.mails().await;
    assert_eq!(mails.len(), 3);
    let parameters = parameters(&mails[0]);
    assert_eq!(parameters["vary-subject-prefix"], "[Accepted]");
    assert_eq!(parameters["mailtags"], json!(["differential-other"]));
    let (trailer_text, trailer_html) = trailer(true);
    assert_eq!(
        parameters["body"],
        format!(
            "bob accepted this revision.\nThis revision is now accepted and ready to land.\n\n\
             {trailer_text}\n\nTo: alice, bob\nCc: dave\n"
        )
    );
    assert_eq!(
        parameters["html-body"],
        format!(
            "{}<br />{trailer_html}<br /><div><strong>To: </strong>alice, bob<br /><strong>Cc: \
             </strong>dave<br /></div>",
            header_table(
                "bob accepted this revision.<br />This revision is now accepted and ready to \
                 land.",
                "http://phab.example.com/D1"
            )
        )
    );
    assert_eq!(
        parameters["stamps"]["3"],
        json!("revision-status(accepted)")
    );

    let (story, _, _) = fixture.story().await.expect("a story");
    assert_eq!(
        text(&story, "storyData"),
        "{\"objectPHID\":\"PHID-DREV-1\",\"transactionPHIDs\":{\"PHID-XACT-DREV-ac\":\
         \"PHID-XACT-DREV-ac\",\"PHID-XACT-DREV-st\":\"PHID-XACT-DREV-st\"}}"
    );
    let ids: Vec<&str> = mails.iter().map(|row| text(row, "id")).collect();
    assert_eq!(
        fixture.queued(task).await,
        expected_tasks(
            search_task("PHID-DREV-1", &xactions),
            Some(text(&story, "chronologicalKey")),
            &ids
        )
    );
    fixture.finish().await;
}

/// alice adds carol as a reviewer: carol joins "To", the title lists her
/// (a handle list, rendered as HTML even in text mail), the mail is about
/// reviewers and "Updated" (strength 100).
#[tokio::test]
async fn publishes_a_reviewer_change_like_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-carol', 'added', 1, 1, '{}')",
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-rv", "PHID-USER-alice", "PHID-DREV-1"),
            "differential.revision.reviewers",
            (
                &json!({"PHID-USER-bob": "added"}),
                &json!({"PHID-USER-bob": "added", "PHID-USER-carol": "added"}),
            ),
            (&json!({}), None),
        )
        .await;
    let state = state(
        "PHID-USER-alice",
        &json!({
            "mailToPHIDs": ["PHID-USER-alice", "PHID-USER-bob", "PHID-USER-carol"],
            "feedNotifyPHIDs": ["PHID-USER-alice", "PHID-USER-bob", "PHID-USER-carol",
                "PHID-USER-dave"],
            "mailStamps": stamps(
                "PHID-USER-alice",
                &[("reviewer", json!(["PHID-USER-bob", "PHID-USER-carol"]))],
            ),
        }),
    );
    let data = data(
        "PHID-DREV-1",
        "PHID-USER-alice",
        &["PHID-XACT-DREV-rv"],
        state,
    );
    let (_, outcome) = fixture.run(config(&json!({})), &data).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    let mails = fixture.mails().await;
    let recipients: Vec<Value> = mails
        .iter()
        .map(|row| {
            let parameters = parameters(row);
            parameters
                .get("to")
                .or_else(|| parameters.get("cc"))
                .cloned()
                .unwrap()
        })
        .collect();
    assert_eq!(
        recipients,
        [
            json!(["PHID-USER-alice"]),
            json!(["PHID-USER-bob"]),
            json!(["PHID-USER-carol"]),
            json!(["PHID-USER-dave"]),
        ]
    );
    let parameters = parameters(&mails[2]);
    assert_eq!(parameters["vary-subject-prefix"], "[Updated]");
    assert_eq!(parameters["mailtags"], json!(["differential-reviewers"]));
    let (trailer_text, _) = trailer(false);
    assert_eq!(
        parameters["body"],
        format!(
            "alice added a reviewer: carol.\n\n{trailer_text}\n\nTo: alice, bob, carol\nCc: \
             dave\n"
        )
    );
    assert!(
        parameters["html-body"]
            .as_str()
            .unwrap()
            .starts_with(&header_table(
                "alice added a reviewer: carol.",
                "http://phab.example.com/D1"
            ))
    );
    assert_eq!(parameters["stamps"]["1"], "reviewer(_@bob)");
    assert_eq!(parameters["stamps"]["2"], "reviewer(_@carol)");
    // alice wrote the story: bob, carol and dave are notified.
    let (_, references, notifications) = fixture.story().await.expect("a story");
    assert_eq!(references, ["PHID-DREV-1", "PHID-USER-alice"]);
    assert_eq!(
        notifications
            .iter()
            .map(|(user, _)| user.as_str())
            .collect::<Vec<_>>(),
        ["PHID-USER-bob", "PHID-USER-carol", "PHID-USER-dave"]
    );
    fixture.finish().await;
}

/// bob comments and Herald (rule H1) subscribes erin: the Herald
/// transaction is in the mail ("Changed Subscribers", strength 75 over the
/// comment's 50) with its header, but not in the story (Herald's
/// subscriptions are hidden from feed). erin gets the mail and the
/// notification.
#[tokio::test]
async fn publishes_a_herald_subscriber_like_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .test_db
        .execute(
            "INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-erin', 1, 1)",
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-c1", "PHID-USER-bob", "PHID-DREV-1"),
            "core:comment",
            (&Value::Null, &Value::Null),
            (&json!({}), Some("Ping.")),
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-hs", HERALD, "PHID-DREV-1"),
            "core:subscribers",
            (
                &json!(["PHID-USER-dave"]),
                &json!(["PHID-USER-dave", "PHID-USER-erin"]),
            ),
            (&json!({}), None),
        )
        .await;
    let xactions = ["PHID-XACT-DREV-c1", "PHID-XACT-DREV-hs"];
    let state = state(
        "PHID-USER-bob",
        &json!({
            "heraldHeader": "<1>",
            "mailCCPHIDs": ["PHID-USER-dave", "PHID-USER-erin"],
            "feedNotifyPHIDs": ["PHID-USER-alice", "PHID-USER-bob", "PHID-USER-dave",
                "PHID-USER-erin"],
            "mailStamps": stamps(
                "PHID-USER-bob",
                &[
                    ("herald", json!(["H1"])),
                    ("subscriber", json!(["PHID-USER-dave", "PHID-USER-erin"])),
                ],
            ),
        }),
    );
    let data = data("PHID-DREV-1", "PHID-USER-bob", &xactions, state);
    let (task, outcome) = fixture.run(config(&json!({})), &data).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    let mails = fixture.mails().await;
    assert_eq!(mails.len(), 4);
    let parameters = parameters(&mails[3]);
    assert_eq!(parameters["cc"], json!(["PHID-USER-erin"]));
    assert_eq!(parameters["vary-subject-prefix"], "[Changed Subscribers]");
    assert_eq!(
        parameters["mailtags"],
        json!(["differential-comment", "differential-cc"])
    );
    assert_eq!(
        parameters["headers"],
        json!([
            ["X-Herald-Rules", "<1>"],
            ["X-Phabricator-To", "<PHID-USER-alice>"],
            ["X-Phabricator-To", "<PHID-USER-bob>"],
            ["X-Phabricator-Cc", "<PHID-USER-dave>"],
            ["X-Phabricator-Cc", "<PHID-USER-erin>"],
        ])
    );
    // The header comes right after the HTML body, before the stamps.
    let keys: Vec<&str> = parameters.keys().map(String::as_str).collect();
    assert_eq!(
        keys[keys.iter().position(|key| *key == "html-body").unwrap()..],
        ["html-body", "headers", "stampMetadata", "stamps", "cc"]
    );
    let (trailer_text, _) = trailer(false);
    assert_eq!(
        parameters["body"],
        format!(
            "bob added a comment.\nHerald added a subscriber: erin.\n\n\n  Ping.\n\n\
             {trailer_text}\n\nTo: alice, bob\nCc: dave, erin\n"
        )
    );
    assert_eq!(parameters["stamps"]["10"], "herald(H1)");

    let (story, _, notifications) = fixture.story().await.expect("a story");
    assert_eq!(
        text(&story, "storyData"),
        "{\"objectPHID\":\"PHID-DREV-1\",\"transactionPHIDs\":{\"PHID-XACT-DREV-c1\":\
         \"PHID-XACT-DREV-c1\"}}"
    );
    assert_eq!(
        notifications
            .iter()
            .map(|(user, _)| user.as_str())
            .collect::<Vec<_>>(),
        ["PHID-USER-alice", "PHID-USER-dave", "PHID-USER-erin"]
    );
    // The search task lists both transactions.
    assert_eq!(
        fixture.queued(task).await[0],
        search_task("PHID-DREV-1", &xactions)
    );
    fixture.finish().await;
}

/// alice creates the draft D2: a draft doesn't broadcast, so the editor
/// left no recipient and nobody to notify: no mail, and a story without
/// notifications. Creating sets the title and the diff from nothing, which
/// feed hides; only "created" shows.
#[tokio::test]
async fn publishes_a_draft_without_mail() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let create = json!({"core.create": true});
    fixture
        .transaction(
            ("PHID-XACT-DREV-cr", "PHID-USER-alice", "PHID-DREV-2"),
            "core:create",
            (&Value::Null, &Value::Null),
            (&create, None),
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-ti", "PHID-USER-alice", "PHID-DREV-2"),
            "differential.revision.title",
            (&json!(""), &json!("Draft")),
            (&create, None),
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-up", "PHID-USER-alice", "PHID-DREV-2"),
            "differential:update",
            (&Value::Null, &json!("PHID-DIFF-2")),
            (&create, None),
        )
        .await;
    let xactions = [
        "PHID-XACT-DREV-cr",
        "PHID-XACT-DREV-ti",
        "PHID-XACT-DREV-up",
    ];
    let state = state(
        "PHID-USER-alice",
        &json!({
            "isNewObject": true,
            "mailToPHIDs": [],
            "mailCCPHIDs": [],
            "feedNotifyPHIDs": [],
            "feedRelatedPHIDs": ["PHID-DREV-2", "PHID-USER-alice"],
            "mailStamps": [],
        }),
    );
    let data = data("PHID-DREV-2", "PHID-USER-alice", &xactions, state);
    let (task, outcome) = fixture.run(config(&json!({})), &data).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    assert_eq!(fixture.mails().await, Vec::<Row>::new());
    let stories = fixture
        .rows(
            "SELECT CAST(storyData AS CHAR) storyData, CAST(chronologicalKey AS CHAR) k \
             FROM `{ns}_feed`.feed_storydata",
        )
        .await;
    assert_eq!(stories.len(), 1);
    assert_eq!(
        text(&stories[0], "storyData"),
        "{\"objectPHID\":\"PHID-DREV-2\",\"transactionPHIDs\":{\"PHID-XACT-DREV-cr\":\
         \"PHID-XACT-DREV-cr\"}}"
    );
    let references = fixture
        .rows(
            "SELECT CAST(objectPHID AS CHAR) objectPHID FROM `{ns}_feed`.feed_storyreference \
             ORDER BY objectPHID",
        )
        .await;
    assert_eq!(
        references
            .iter()
            .map(|row| text(row, "objectPHID"))
            .collect::<Vec<_>>(),
        ["PHID-DREV-2", "PHID-USER-alice"]
    );
    assert_eq!(
        fixture
            .rows("SELECT userPHID FROM `{ns}_feed`.feed_storynotification")
            .await,
        Vec::<Row>::new()
    );
    let mut expected = expected_tasks(
        search_task("PHID-DREV-2", &xactions),
        Some(text(&stories[0], "k")),
        &[],
    );
    expected[0].2 = Some("PHID-DREV-2".to_owned());
    assert_eq!(fixture.queued(task).await, expected);
    fixture.finish().await;
}

/// A comment on a secure revision (Mozilla's "Secure Mail" rule H2 asked
/// for encryption): the mail is the same, flagged `mustEncrypt` (in its
/// original place) with the rule as the reason (last), and the
/// `#secure-revision` project shows in the headers and the stamps.
#[tokio::test]
async fn publishes_a_secure_revision_like_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .test_db
        .execute(
            "INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 41, 'PHID-PROJ-secure', 1, 0)",
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-c1", "PHID-USER-bob", "PHID-DREV-1"),
            "core:comment",
            (&Value::Null, &Value::Null),
            (&json!({}), Some("Looks good to me.")),
        )
        .await;
    let state = state(
        "PHID-USER-bob",
        &json!({
            "heraldHeader": "<1>, <2>",
            "mustEncrypt": ["PHID-HRUL-two"],
            "feedRelatedPHIDs": ["PHID-DREV-1", "PHID-USER-bob", "PHID-PROJ-secure"],
            "mailStamps": stamps(
                "PHID-USER-bob",
                &[
                    ("encrypted", json!(["PHID-HRUL-two"])),
                    ("herald", json!(["H1", "H2"])),
                    ("tag", json!(["PHID-PROJ-secure"])),
                ],
            ),
        }),
    );
    let data = data(
        "PHID-DREV-1",
        "PHID-USER-bob",
        &["PHID-XACT-DREV-c1"],
        state,
    );
    let (_, outcome) = fixture.run(config(&json!({})), &data).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    let mails = fixture.mails().await;
    assert_eq!(mails.len(), 3);
    for row in &mails {
        let parameters = parameters(row);
        let keys: Vec<&str> = parameters.keys().map(String::as_str).collect();
        assert_eq!(keys[..2], ["sensitive", "mustEncrypt"]);
        assert_eq!(keys.last(), Some(&"mustEncryptReasons"));
        assert_eq!(parameters["mustEncrypt"], true);
        assert_eq!(parameters["mustEncryptReasons"], json!(["PHID-HRUL-two"]));
        assert_eq!(
            parameters["headers"][0],
            json!(["X-Herald-Rules", "<1>, <2>"])
        );
        assert_eq!(
            parameters["headers"][1],
            json!(["X-Phabricator-Projects", "<#secure-revision>"])
        );
        // Stamps: author 0, reviewer 1, repository 2, status 3,
        // application 4, monogram 5, phid 6, object type 7, actor 8, via 9,
        // encrypted 10, herald 11 and 12, tag 13, subscriber 14.
        assert_eq!(
            parameters["stamps"],
            json!({
                "8": "actor(_@bob)",
                "4": "application(Differential)",
                "0": "author(_@alice)",
                "10": "encrypted()",
                "11": "herald(H1)",
                "12": "herald(H2)",
                "5": "monogram(D1)",
                "7": "object-type(DREV)",
                "6": "phid(PHID-DREV-1)",
                "1": "reviewer(_@bob)",
                "2": "revision-repository(rVCT)",
                "3": "revision-status(needs-review)",
                "14": "subscriber(_@dave)",
                "13": "tag(_#secure-revision)",
                "9": "via(conduit)",
            })
        );
        assert!(
            parameters["body"]
                .as_str()
                .unwrap()
                .starts_with("bob added a comment.\n\n\n  Looks good to me.\n\nREPOSITORY")
        );
    }
    let (_, references, _) = fixture.story().await.expect("a story");
    assert_eq!(
        references,
        ["PHID-DREV-1", "PHID-PROJ-secure", "PHID-USER-bob"]
    );
    fixture.finish().await;
}

/// `loadObject()` and `loadTransactions()` fail permanently when the
/// revision or a transaction does not load.
#[tokio::test]
async fn fails_permanently_like_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    for (data, message) in [
        (
            json!({"objectPHID": "PHID-DREV-gone", "actorPHID": "PHID-USER-bob"}),
            "Unable to load object with PHID \"PHID-DREV-gone\"!",
        ),
        (
            data(
                "PHID-DREV-1",
                "PHID-USER-bob",
                &["PHID-XACT-DREV-gone"],
                state("PHID-USER-bob", &json!({})),
            ),
            "Unable to load transactions: PHID-XACT-DREV-gone.",
        ),
        (
            json!({"actorPHID": "PHID-USER-bob"}),
            "Task has no object PHID!",
        ),
        (json!("x"), "Task has invalid task data."),
    ] {
        let (_, outcome) = fixture.run(config(&json!({})), &data).await;
        let Executed::FailedPermanently { message: got, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(got, message);
    }
    assert_eq!(fixture.mails().await, Vec::<Row>::new());
    fixture.finish().await;
}

/// Tasks Rust can't publish as PHP does are never leased: other objects,
/// mentions in comments, firehose webhooks, outbound mail rules. Nothing
/// is written.
#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .transaction(
            ("PHID-XACT-DREV-c1", "PHID-USER-bob", "PHID-DREV-1"),
            "core:comment",
            (&Value::Null, &Value::Null),
            (&json!({}), Some("Thanks @alice, see D2.")),
        )
        .await;
    fixture
        .transaction(
            ("PHID-XACT-DREV-c2", "PHID-USER-bob", "PHID-DREV-1"),
            "core:comment",
            (&Value::Null, &Value::Null),
            (&json!({}), Some("Fine.")),
        )
        .await;
    let comment = |phid: &str| {
        data(
            "PHID-DREV-1",
            "PHID-USER-bob",
            &[phid],
            state("PHID-USER-bob", &json!({})),
        )
    };
    let worker = fixture.worker(config(&json!({})));
    let mut ids = Vec::new();
    for data in [
        json!({"objectPHID": "PHID-PROJ-secure", "actorPHID": "PHID-USER-bob"}),
        comment("PHID-XACT-DREV-c1"),
        data(
            "PHID-DREV-1",
            "PHID-USER-bob",
            &["PHID-XACT-DREV-c2"],
            state(
                "PHID-USER-bob",
                &json!({"webhookMap": {"PHID-HWBH-1": ["x"]}}),
            ),
        ),
    ] {
        ids.push(fixture.schedule(&data).await);
    }
    assert!(worker.work_on(None).await.unwrap().is_none());

    // Firehose hooks, then outbound mail rules.
    fixture
        .test_db
        .execute(
            "INSERT INTO `{ns}_herald`.herald_webhook (phid, name, webhookURI, viewPolicy, \
               editPolicy, status, hmacKey, dateCreated, dateModified) VALUES \
             ('PHID-HWBH-1', 'All', 'https://hook/', 'users', 'admin', 'firehose', 'k', 1, 1)",
        )
        .await;
    let firehose = fixture.schedule(&comment("PHID-XACT-DREV-c2")).await;
    let worker = fixture.worker(config(&json!({})));
    assert!(worker.work_on(Some(&[firehose])).await.unwrap().is_none());
    fixture
        .test_db
        .execute(
            "DELETE FROM `{ns}_herald`.herald_webhook; \
             INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, \
               ruleType, phid, isDisabled, triggerObjectPHID) VALUES \
             (9, 'Route', 'PHID-USER-alice', 'PhabricatorMailOutboundMailHeraldAdapter', 1, 38, \
               1, 1, 'every', 'global', 'PHID-HRUL-nine', 0, NULL)",
        )
        .await;
    let worker = fixture.worker(config(&json!({})));
    assert!(worker.work_on(Some(&[firehose])).await.unwrap().is_none());

    assert_eq!(fixture.mails().await, Vec::<Row>::new());
    assert_eq!(
        fixture
            .rows("SELECT id FROM `{ns}_feed`.feed_storydata")
            .await,
        Vec::<Row>::new()
    );
    let leased = fixture
        .rows(
            "SELECT id FROM `{ns}_worker`.worker_activetask WHERE leaseOwner IS NOT NULL \
             OR taskClass != 'PhabricatorApplicationTransactionPublishWorker'",
        )
        .await;
    assert_eq!(leased, Vec::<Row>::new());
    fixture.finish().await;
}
