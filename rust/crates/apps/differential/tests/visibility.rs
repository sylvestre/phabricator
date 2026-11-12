use phab_app_differential::{
    Repositories,
    storage::{load_comments, load_revisions, load_transactions},
};
use phab_core::{Config, Phid};
use phab_db::testing::TestDatabase;
use phab_policy::{Capability, PolicySubject, Viewer, filter, loader::load_facts};

const DATABASES: &[&str] = &[
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
];

async fn visible(test_db: &TestDatabase, viewer: &Viewer, phids: &[&str]) -> Vec<bool> {
    let database = test_db.database();
    let phids: Vec<Phid> = phids.iter().map(|phid| Phid::new(*phid)).collect();
    let mut revisions = load_revisions(database, &phids).await.unwrap();
    revisions.sort_by_key(|revision| {
        phids
            .iter()
            .position(|phid| *phid == revision.phid)
            .unwrap_or(usize::MAX)
    });
    let subjects: Vec<PolicySubject> = revisions
        .iter()
        .map(phab_app_differential::Revision::subject)
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts = load_facts(
        database,
        &Config::default(),
        viewer,
        &refs,
        &Repositories { database },
        0,
    )
    .await
    .unwrap();
    filter(viewer, &refs, &[Capability::VIEW], &facts).unwrap()
}

#[tokio::test]
async fn revisions_need_their_repository() {
    let Some(test_db) = TestDatabase::create(DATABASES).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository (phid, name, versionControlSystem, details, \
               dateCreated, dateModified, viewPolicy, editPolicy, pushPolicy) VALUES \
             ('PHID-REPO-open', 'open', 'hg', '{}', 1, 1, 'users', 'admin', 'admin'), \
             ('PHID-REPO-secret', 'secret', 'hg', '{}', 1, 1, 'admin', 'admin', 'admin'); \
             INSERT INTO `{ns}_differential`.differential_revision (title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             ('a', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', 'PHID-REPO-open', '{}', ''), \
             ('b', 'PHID-DREV-secret', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', 'PHID-REPO-secret', '{}', ''), \
             ('c', 'PHID-DREV-private', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'obj.subscriptions.subscribers', 'users', NULL, '{}', ''), \
             ('d', 'PHID-DREV-orphan', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', 'PHID-REPO-deleted', '{}', ''); \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-private', 'PHID-USER-bob', 'accepted', 1, 1, '{}'), \
             ('PHID-DREV-private', 'PHID-USER-carol', 'resigned', 1, 1, '{}')",
        )
        .await;

    let all = [
        "PHID-DREV-open",
        "PHID-DREV-secret",
        "PHID-DREV-private",
        "PHID-DREV-orphan",
    ];
    let alice = Viewer::user(Phid::new("PHID-USER-alice"), false);
    let bob = Viewer::user(Phid::new("PHID-USER-bob"), false);
    let carol = Viewer::user(Phid::new("PHID-USER-carol"), false);
    let admin = Viewer::user(Phid::new("PHID-USER-root"), true);
    // Even the author needs to see the repository.
    assert_eq!(
        visible(&test_db, &alice, &all).await,
        [true, false, true, false]
    );
    // Reviewers are implicit subscribers, unless they resigned.
    assert_eq!(
        visible(&test_db, &bob, &all).await,
        [true, false, true, false]
    );
    assert_eq!(
        visible(&test_db, &carol, &all).await,
        [true, false, false, false]
    );
    assert_eq!(
        visible(&test_db, &admin, &all).await,
        [true, true, false, false]
    );
    assert_eq!(
        visible(&test_db, &Viewer::anonymous(), &all).await,
        [false, false, false, false]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn loads_transactions_and_comments() {
    let Some(test_db) = TestDatabase::create(DATABASES).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction (phid, authorPHID, objectPHID, \
               viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, \
               newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-DREV-1', 'PHID-USER-alice', 'PHID-DREV-1', 'public', 'PHID-USER-alice', \
               'PHID-XCMT-1', 1, 'core:comment', 'null', 'null', '{}', '{\"core.create\":true}', 1, 1); \
             INSERT INTO `{ns}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, lineLength, \
               hasReplies) VALUES \
             ('PHID-XCMT-1', 'PHID-XACT-DREV-1', 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, \
               'LGTM', '{}', 0, 1, 1, 0, 0, 0, 0)",
        )
        .await;
    let database = test_db.database();
    let transactions = load_transactions(database, &[Phid::new("PHID-XACT-DREV-1")])
        .await
        .unwrap();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].transaction_type, "core:comment");
    assert_eq!(transactions[0].comment_phid, Some(Phid::new("PHID-XCMT-1")));
    assert_eq!(
        transactions[0].metadata_value("core.create"),
        Some(&serde_json::json!(true))
    );
    let comments = load_comments(database, &[Phid::new("PHID-XCMT-1")])
        .await
        .unwrap();
    assert_eq!(comments[0].content, "LGTM");
    assert!(!comments[0].is_empty());

    test_db.drop_databases().await;
}
