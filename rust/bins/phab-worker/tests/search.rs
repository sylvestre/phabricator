//! `PhabricatorSearchWorker` against a throwaway `MySQL` database: the rows
//! it writes, compared to those `PhabricatorIndexEngine` and
//! `PhabricatorFerretFulltextEngineExtension` write (derived from the PHP
//! code, see each test), and the tasks it leaves to PHP untouched.
//!
//! The corpora and ngrams themselves are checked against PHP's output by
//! `crates/apps/search/tests/oracle.rs`; here they come from
//! [`ferret::build`] of the fields PHP's document has.

use phab_app_search::ferret;
use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, rows};
use phab_worker::handlers::search::SearchWorker;
use phab_worker::{Executed, LeaseOwner, Registry, Worker};
use serde_json::{Value, json};

const CLASS: &str = "PhabricatorSearchWorker";
const DATABASES: &[&str] = &["worker", "differential", "user", "project", "search"];
const REVISION: &str = "PHID-DREV-one";
const NOW: i64 = 1_700_000_000;

struct Fixture {
    test_db: TestDatabase,
}

async fn fixture() -> Option<Fixture> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (1, 'PHID-USER-alice', 'alice', 'Alice Liddell', 10, 20, 'c', 0, 0, 0, 1, 1, 's', \
               0, 0), \
             (2, 'PHID-USER-bob', 'bob', '', 11, 21, 'c', 0, 1, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Fix the crash when loading pages', 'PHID-DREV-one', 'needs-review', \
               'Loading crashed in nsDocLoader::Stop().', 'Ran the tests.', 'PHID-USER-alice', \
               100, 200, '{}', 'k', 'users', 'users', NULL, '{}', ''); \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-one', 'PHID-USER-bob', 'added', 1, 1, '{}'), \
             ('PHID-DREV-one', 'PHID-USER-carol', 'resigned', 1, 1, '{}')",
        )
        .await;
    Some(Fixture { test_db })
}

type Row = Vec<Option<String>>;

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    async fn execute(&self, sql: &str) {
        self.test_db.execute(sql).await;
    }

    /// A revision transaction, with a comment if `comment` is given.
    async fn comment(&self, id: i64, comment: Option<&str>) {
        let comment_phid = match comment {
            Some(_) => format!("'PHID-XCMT-{id}'"),
            None => "NULL".to_owned(),
        };
        self.execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_transaction (id, phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-XACT-DREV-{id}', 'PHID-USER-alice', 'PHID-DREV-one', 'public', \
               'public', {comment_phid}, 1, 'core:comment', 'null', 'null', '{{}}', '{{}}', 1, 1)"
        ))
        .await;
        if let Some(content) = comment {
            let sql = format!(
                "INSERT INTO {} (id, phid, transactionPHID, authorPHID, viewPolicy, editPolicy, \
                   commentVersion, content, contentSource, isDeleted, dateCreated, \
                   dateModified, revisionPHID, changesetID, isNewFile, lineNumber, lineLength, \
                   fixedState, hasReplies, replyToCommentPHID, attributes) VALUES \
                 (?, ?, ?, 'PHID-USER-alice', 'public', 'public', 1, ?, '{{}}', 0, 1, 1, \
                   'PHID-DREV-one', NULL, 0, 0, 0, NULL, 0, NULL, '{{}}')",
                self.database()
                    .table("differential", "differential_transaction_comment")
            );
            sqlx::query(&sql)
                .bind(id + 100)
                .bind(format!("PHID-XCMT-{id}"))
                .bind(format!("PHID-XACT-DREV-{id}"))
                .bind(content)
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
                priority: 3500,
                object_phid: None,
            },
            NOW,
        )
        .await
        .unwrap();
        i64::try_from(id).unwrap()
    }

    fn worker(&self, config: Config) -> Worker {
        let mut registry = Registry::default();
        registry.register(SearchWorker);
        Worker::new(
            self.database().clone(),
            config,
            registry,
            LeaseOwner::new("rust-test"),
        )
    }

    /// Queues an indexing task of `phid` and runs it.
    async fn index(&self, phid: &str, parameters: Value, config: Config) -> Executed {
        let id = self
            .schedule(&json!({"documentPHID": phid, "parameters": parameters, "strict": false}))
            .await;
        let ran = self
            .worker(config)
            .work_on(Some(&[id]))
            .await
            .unwrap()
            .expect("a task to run");
        assert_eq!(ran.id, id);
        ran.outcome.unwrap()
    }

    /// Every row of a query, each column as text.
    async fn rows(&self, sql: &str) -> Vec<Row> {
        let sql = sql.replace("{ns}", self.database().namespace());
        sqlx::query(&sql)
            .fetch_all(self.database().pool())
            .await
            .unwrap()
            .iter()
            .map(|row| {
                use sqlx::Row as _;
                (0..row.len())
                    .map(|index| {
                        let column = row.column_name(index);
                        rows::opt_text(row, &column).unwrap_or_else(|_| {
                            rows::opt_int(row, &column)
                                .unwrap()
                                .map(|number| number.to_string())
                        })
                    })
                    .collect()
            })
            .collect()
    }

    async fn count(&self, table: &str) -> usize {
        self.rows(&format!("SELECT id FROM {table}")).await.len()
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

trait ColumnName {
    fn column_name(&self, index: usize) -> String;
}

impl ColumnName for sqlx::mysql::MySqlRow {
    fn column_name(&self, index: usize) -> String {
        use sqlx::{Column, Row as _};
        self.columns()[index].name().to_owned()
    }
}

#[allow(clippy::unnecessary_wraps)] // Nullable columns are `Option`s.
fn text(value: &str) -> Option<String> {
    Some(value.to_owned())
}

fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, corpus)| ((*key).to_owned(), (*corpus).to_owned()))
        .collect()
}

/// The `*_ffield` rows of `corpus`, as `(fieldKey, raw, term, normal)`.
fn field_rows(corpus: &ferret::Corpus) -> Vec<Row> {
    corpus
        .fields
        .iter()
        .map(|field| {
            vec![
                text(&field.key),
                text(&field.raw),
                text(&field.term),
                text(&field.normal),
            ]
        })
        .collect()
}

async fn stored_fields(fixture: &Fixture, prefix: &str) -> Vec<Row> {
    fixture
        .rows(&format!(
            "SELECT fieldKey, rawCorpus, termCorpus, normalCorpus FROM {prefix}_ffield ORDER BY id"
        ))
        .await
}

async fn stored_ngrams(fixture: &Fixture, prefix: &str) -> Vec<String> {
    fixture
        .rows(&format!("SELECT ngram FROM {prefix}_fngrams ORDER BY id"))
        .await
        .into_iter()
        .map(|row| row[0].clone().unwrap())
        .collect()
}

/// What a `CHAR(3)` column gives back.
fn trimmed(ngrams: &[String]) -> Vec<String> {
    ngrams
        .iter()
        .map(|ngram| ngram.trim_end_matches(' ').to_owned())
        .collect()
}

fn succeeded(executed: &Executed) {
    assert!(matches!(executed, Executed::Succeeded(_)), "{executed:?}");
}

const REVISION_TABLES: &str = "`{ns}_differential`.differential_revision";

/// `DifferentialRevisionFulltextEngine` then the custom fields (summary,
/// test plan) and the comments, newest first; empty comments are left out.
/// Owner: the last reviewer of a revision which needs review, resigned or
/// not.
#[tokio::test]
async fn indexes_a_revision() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture.comment(1, Some("First comment")).await;
    fixture.comment(2, None).await;
    fixture.comment(3, Some("  \n ")).await;
    fixture.comment(4, Some("Looks good, landing")).await;
    // A common ngram is not stored.
    fixture
        .execute(
            "INSERT INTO `{ns}_differential`.differential_revision_fngrams_common \
               (ngram, needsCollection) VALUES ('the', 0), (' ru', 0)",
        )
        .await;

    succeeded(&fixture.index(REVISION, json!([]), Config::default()).await);
    let prefix = "`{ns}_differential`.differential_revision";
    assert_eq!(
        fixture
            .rows(&format!(
                "SELECT id, objectPHID, isClosed, authorPHID, ownerPHID, epochCreated, \
                   epochModified FROM {prefix}_fdocument"
            ))
            .await,
        [vec![
            text("1"),
            text(REVISION),
            text("0"),
            text("PHID-USER-alice"),
            text("PHID-USER-carol"),
            text("100"),
            text("200"),
        ]]
    );
    let corpus = ferret::build(&fields(&[
        ("titl", "Fix the crash when loading pages"),
        ("body", "Loading crashed in nsDocLoader::Stop()."),
        ("plan", "Ran the tests."),
        ("cmnt", "Looks good, landing"),
        ("cmnt", "First comment"),
    ]))
    .unwrap();
    assert_eq!(stored_fields(&fixture, prefix).await, field_rows(&corpus));
    let expected: Vec<String> = corpus
        .ngrams
        .iter()
        .filter(|ngram| !["the", " ru"].contains(&ngram.trim_end_matches(' ')))
        .cloned()
        .collect();
    assert!(expected.len() < corpus.ngrams.len());
    assert_eq!(stored_ngrams(&fixture, prefix).await, trimmed(&expected));

    // `getIndexVersion()`: the configuration's hash, the newest transaction
    // and comment IDs.
    let config_hash = phab_core::hash::digest_for_index(
        br#"{"services":[{"type":"mysql","roles":{"read":true,"write":true}}]}"#,
    );
    let versions = fixture
        .rows(
            "SELECT objectPHID, extensionKey, version, indexVersion, indexEpoch \
             FROM `{ns}_search`.search_indexversion",
        )
        .await;
    assert_eq!(versions.len(), 1);
    assert_eq!(
        versions[0][..4],
        [
            text(REVISION),
            text("fulltext"),
            Some(format!("{config_hash}:4:104")),
            text("2021-02-16-A"),
        ]
    );

    // Nothing changed: the index is current and nothing is written.
    fixture
        .execute(&format!(
            "UPDATE {REVISION_TABLES} SET title = 'Something else'; \
             UPDATE `{{ns}}_search`.search_indexversion SET indexEpoch = 5"
        ))
        .await;
    succeeded(&fixture.index(REVISION, json!([]), Config::default()).await);
    assert_eq!(stored_fields(&fixture, prefix).await, field_rows(&corpus));
    assert_eq!(
        fixture
            .rows("SELECT indexEpoch FROM `{ns}_search`.search_indexversion")
            .await,
        [vec![text("5")]]
    );
    fixture.finish().await;
}

/// `force` rebuilds a current index: the document row is updated, changed
/// fields are updated in place, new ones inserted, gone ones deleted, and
/// only the ngrams which changed are deleted or inserted.
#[tokio::test]
async fn updates_an_indexed_revision() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture.comment(1, Some("First comment")).await;
    succeeded(&fixture.index(REVISION, json!([]), Config::default()).await);
    let prefix = "`{ns}_differential`.differential_revision";
    let before = fixture
        .rows(&format!(
            "SELECT id, fieldKey FROM {prefix}_ffield ORDER BY id"
        ))
        .await;
    let ngrams_before = fixture
        .rows(&format!(
            "SELECT id, ngram FROM {prefix}_fngrams ORDER BY id"
        ))
        .await;

    fixture
        .execute(&format!(
            "UPDATE {REVISION_TABLES} SET title = 'Fix the crash when loading', \
               testPlan = '', status = 'accepted', dateModified = 300"
        ))
        .await;
    succeeded(
        &fixture
            .index(REVISION, json!({"force": true}), Config::default())
            .await,
    );
    let corpus = ferret::build(&fields(&[
        ("titl", "Fix the crash when loading"),
        ("body", "Loading crashed in nsDocLoader::Stop()."),
        ("cmnt", "First comment"),
    ]))
    .unwrap();
    assert_eq!(
        fixture
            .rows(&format!(
                "SELECT id, isClosed, ownerPHID, epochModified FROM {prefix}_fdocument"
            ))
            .await,
        // An accepted revision is owned by its author.
        [vec![
            text("1"),
            text("0"),
            text("PHID-USER-alice"),
            text("300")
        ]]
    );
    let after = fixture
        .rows(&format!(
            "SELECT id, fieldKey FROM {prefix}_ffield ORDER BY id"
        ))
        .await;
    // "plan" is gone; the others keep their IDs.
    let kept: Vec<&Row> = before
        .iter()
        .filter(|row| row[1].as_deref() != Some("plan"))
        .collect();
    assert_eq!(after.iter().collect::<Vec<_>>(), kept);
    let mut stored = stored_fields(&fixture, prefix).await;
    let mut expected = field_rows(&corpus);
    stored.sort();
    expected.sort();
    assert_eq!(stored, expected);

    // The ngrams which stay keep their rows; new ones come after.
    let ngrams_after = fixture
        .rows(&format!(
            "SELECT id, ngram FROM {prefix}_fngrams ORDER BY id"
        ))
        .await;
    let wanted = trimmed(&corpus.ngrams);
    let kept: Vec<&Row> = ngrams_before
        .iter()
        .filter(|row| wanted.contains(row[1].as_ref().unwrap()))
        .collect();
    assert_eq!(ngrams_after[..kept.len()].iter().collect::<Vec<_>>(), kept);
    let mut all: Vec<String> = ngrams_after
        .iter()
        .map(|row| row[1].clone().unwrap())
        .collect();
    all.sort();
    let mut wanted_sorted = wanted.clone();
    wanted_sorted.sort();
    assert_eq!(all, wanted_sorted);
    let new_ngrams: Vec<String> = wanted
        .iter()
        .filter(|ngram| {
            !ngrams_before
                .iter()
                .any(|row| row[1].as_ref() == Some(*ngram))
        })
        .cloned()
        .collect();
    assert_eq!(
        ngrams_after[kept.len()..]
            .iter()
            .map(|row| row[1].clone().unwrap())
            .collect::<Vec<_>>(),
        new_ngrams
    );
    fixture.finish().await;
}

/// `PhabricatorUserFulltextEngine`: the title is `getFullName()`, a
/// disabled user is closed; users have no comments.
#[tokio::test]
async fn indexes_users() {
    let Some(fixture) = fixture().await else {
        return;
    };
    succeeded(
        &fixture
            .index("PHID-USER-alice", json!([]), Config::default())
            .await,
    );
    succeeded(
        &fixture
            .index("PHID-USER-bob", json!([]), Config::default())
            .await,
    );
    let prefix = "`{ns}_user`.user_user";
    assert_eq!(
        fixture
            .rows(&format!(
                "SELECT objectPHID, isClosed, authorPHID, ownerPHID, epochCreated, \
                   epochModified FROM {prefix}_fdocument ORDER BY id"
            ))
            .await,
        [
            vec![
                text("PHID-USER-alice"),
                text("0"),
                None,
                None,
                text("10"),
                text("20")
            ],
            vec![
                text("PHID-USER-bob"),
                text("1"),
                None,
                None,
                text("11"),
                text("21")
            ],
        ]
    );
    let alice = ferret::build(&fields(&[("titl", "alice (Alice Liddell)")])).unwrap();
    let bob = ferret::build(&fields(&[("titl", "bob")])).unwrap();
    let mut expected = field_rows(&alice);
    expected.extend(field_rows(&bob));
    assert_eq!(stored_fields(&fixture, prefix).await, expected);
    let mut ngrams = trimmed(&alice.ngrams);
    ngrams.extend(trimmed(&bob.ngrams));
    assert_eq!(stored_ngrams(&fixture, prefix).await, ngrams);
    let config_hash = phab_core::hash::digest_for_index(
        br#"{"services":[{"type":"mysql","roles":{"read":true,"write":true}}]}"#,
    );
    assert_eq!(
        fixture
            .rows("SELECT version FROM `{ns}_search`.search_indexversion ORDER BY id")
            .await,
        [
            vec![Some(format!("{config_hash}:none:none"))],
            vec![Some(format!("{config_hash}:none:none"))],
        ]
    );
    fixture.finish().await;
}

/// `PhabricatorProjectFulltextEngine`: the datasource tokens are rebuilt,
/// the slugs are a body field and the description another; the
/// membership extension materializes the members and clears
/// `hasSubprojects` and `hasMilestones`; it has no version, so the project
/// is indexed again even when its fulltext index is current.
#[tokio::test]
async fn indexes_projects() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let index = phab_core::hash::digest_for_index(b"std:project:internal:description");
    fixture
        .execute(&format!(
            "INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug, parentProjectPHID, \
               milestoneNumber) VALUES \
             (1, 'Web Platform', 'PHID-PROJ-web', 'x', 30, 40, '100', 'users', 'users', 'users', \
               'fa-cog', 'blue', 'k', 0, 1, 1, 'a', 0, 'k001', '{{}}', 'default', 'web', NULL, \
               NULL); \
             INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, \
               dateModified) VALUES ('PHID-PROJ-web', 'web', 1, 1), \
               ('PHID-PROJ-web', 'web-platform', 1, 1); \
             INSERT INTO `{{ns}}_project`.project_customfieldstorage (objectPHID, fieldIndex, \
               fieldValue) VALUES ('PHID-PROJ-web', '{index}', 'Layout and DOM.'); \
             INSERT INTO `{{ns}}_project`.project_datasourcetoken (projectID, token) VALUES \
               (1, 'stale'); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-PROJ-web', 13, 'PHID-USER-alice', 5, 0), \
               ('PHID-PROJ-web', 13, 'PHID-USER-bob', 6, 1), \
               ('PHID-PROJ-web', 60, 'PHID-USER-bob', 6, 1), \
               ('PHID-PROJ-web', 60, 'PHID-USER-gone', 7, 0)"
        ))
        .await;

    succeeded(
        &fixture
            .index("PHID-PROJ-web", json!([]), Config::default())
            .await,
    );
    assert_eq!(
        fixture
            .rows(
                "SELECT token FROM `{ns}_project`.project_datasourcetoken \
                 WHERE projectID = 1 ORDER BY id"
            )
            .await,
        [vec![text("web")], vec![text("platform")]]
    );
    let prefix = "`{ns}_project`.project_project";
    assert_eq!(
        fixture
            .rows(&format!(
                "SELECT objectPHID, isClosed, authorPHID, ownerPHID FROM {prefix}_fdocument"
            ))
            .await,
        [vec![text("PHID-PROJ-web"), text("1"), None, None]]
    );
    let corpus = ferret::build(&fields(&[
        ("titl", "Web Platform"),
        ("body", "web\nweb-platform"),
        ("body", "Layout and DOM."),
    ]))
    .unwrap();
    assert_eq!(stored_fields(&fixture, prefix).await, field_rows(&corpus));
    assert_eq!(
        fixture
            .rows(
                "SELECT dst, dateCreated, seq FROM `{ns}_project`.edge \
                 WHERE src = 'PHID-PROJ-web' AND type = 60 ORDER BY dst"
            )
            .await,
        [
            vec![text("PHID-USER-alice"), text("5"), text("0")],
            vec![text("PHID-USER-bob"), text("6"), text("1")],
        ]
    );
    assert_eq!(
        fixture
            .rows("SELECT hasSubprojects, hasMilestones FROM `{ns}_project`.project")
            .await,
        [vec![text("0"), text("0")]]
    );

    // Current fulltext index: only the members and the version's epoch.
    fixture
        .execute(
            "DELETE FROM `{ns}_project`.edge WHERE type = 60; \
             UPDATE `{ns}_project`.project SET name = 'Renamed'; \
             UPDATE `{ns}_search`.search_indexversion SET indexEpoch = 5",
        )
        .await;
    succeeded(
        &fixture
            .index("PHID-PROJ-web", json!([]), Config::default())
            .await,
    );
    assert_eq!(stored_fields(&fixture, prefix).await, field_rows(&corpus));
    assert_eq!(
        fixture
            .rows("SELECT COUNT(*) FROM `{ns}_project`.edge WHERE type = 60")
            .await,
        [vec![text("2")]]
    );
    let epoch = fixture
        .rows("SELECT indexEpoch FROM `{ns}_search`.search_indexversion")
        .await;
    assert_ne!(epoch, [vec![text("5")]]);
    fixture.finish().await;
}

/// `loadObjectForIndexing()` fails: nothing is written, and the task
/// fails only in strict mode.
#[tokio::test]
async fn missing_objects_fail_only_in_strict_mode() {
    let Some(fixture) = fixture().await else {
        return;
    };
    succeeded(
        &fixture
            .index("PHID-DREV-gone", json!([]), Config::default())
            .await,
    );
    let id = fixture
        .schedule(&json!({"documentPHID": "PHID-DREV-gone", "strict": true}))
        .await;
    let ran = fixture
        .worker(Config::default())
        .work_on(Some(&[id]))
        .await
        .unwrap()
        .unwrap();
    match ran.outcome.unwrap() {
        Executed::FailedPermanently { message, .. } => assert_eq!(
            message,
            "Unable to load object \"PHID-DREV-gone\" to rebuild indexes."
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.count("`{ns}_search`.search_indexversion").await, 0);
    fixture.finish().await;
}

/// Another process indexing the object holds the lock: the task yields
/// for 15 seconds.
#[tokio::test]
async fn yields_while_the_object_is_locked() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let name = phab_app_search::index::lock_name(fixture.database().namespace(), REVISION);
    let mut holder = fixture.database().pool().acquire().await.unwrap();
    let _: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, 0)")
        .bind(&name)
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let executed = fixture.index(REVISION, json!([]), Config::default()).await;
    assert!(matches!(executed, Executed::Yielded(_)), "{executed:?}");
    assert_eq!(
        fixture
            .count("`{ns}_differential`.differential_revision_fdocument")
            .await,
        0
    );
    fixture.finish().await;
}

/// What Rust doesn't index is left to PHP before the lease: other object
/// types, subprojects, text whose case folding beyond ASCII may differ,
/// configured custom fields.
#[tokio::test]
async fn leaves_what_is_not_ported_to_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .execute(
            "INSERT INTO `{ns}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug, parentProjectPHID, \
               milestoneNumber) VALUES \
             (1, 'Parent', 'PHID-PROJ-parent', 'x', 1, 1, '0', 'users', 'users', 'users', \
               'fa-x', 'red', 'k', 0, 0, 1, 'a', 0, 'k001', '{}', 'default', NULL, NULL, NULL), \
             (2, 'Child', 'PHID-PROJ-child', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'red', 'k', 0, 0, 0, 'ab', 1, 'k002', '{}', 'default', NULL, 'PHID-PROJ-parent', \
               NULL); \
             UPDATE `{ns}_user`.user SET realName = 'Élodie' WHERE id = 1",
        )
        .await;
    let mut custom = Config::default();
    let mut layer = serde_json::Map::new();
    layer.insert(
        "differential.custom-field-definitions".to_owned(),
        json!({"x": {"type": "text"}}),
    );
    custom.push_layer("test", layer);
    for (phid, config) in [
        ("PHID-TASK-x", Config::default()),
        ("PHID-PROJ-parent", Config::default()),
        ("PHID-PROJ-child", Config::default()),
        ("PHID-USER-alice", Config::default()),
        (REVISION, custom),
    ] {
        let id = fixture
            .schedule(&json!({"documentPHID": phid, "parameters": []}))
            .await;
        let worker = fixture.worker(config);
        assert!(
            worker.work_on(Some(&[id])).await.unwrap().is_none(),
            "{phid}"
        );
        assert_eq!(
            fixture
                .rows(&format!(
                    "SELECT leaseOwner FROM `{{ns}}_worker`.worker_activetask WHERE id = {id}"
                ))
                .await,
            [vec![None]],
            "{phid}"
        );
    }
    assert_eq!(fixture.count("`{ns}_search`.search_indexversion").await, 0);
    fixture.finish().await;
}
