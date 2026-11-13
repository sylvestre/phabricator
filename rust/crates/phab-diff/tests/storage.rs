//! Loading diffs, changesets and hunks from a test database.

use phab_db::testing::TestDatabase;
use phab_diff::storage::{self, StorageError};
use phab_diff::{ChangeType, FileType};

/// `gzdeflate(" a\n-b\n+c\n")`, from PHP.
const DEFLATED_HEX: &str = "5348e4d24de2d24ee60200";

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&["differential", "file"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlBaseRevision, lintStatus, unitStatus, \
               lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (7, 'PHID-DIFF-seven', 3, 'PHID-USER-alice', NULL, 'abc123', 0, 0, 4, 100, 100, \
               'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, phid, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified) VALUES \
             (10, 'PHID-DCHG-ten', 7, NULL, 'a.c', '[]', 2, 1, '{{\"line:first\":2}}', '[]', \
               '[]', 3, 2, 1, 1), \
             (11, 'PHID-DCHG-eleven', 7, 'old.c', 'new.c', NULL, 6, 1, NULL, NULL, NULL, 0, 0, \
               1, 1), \
             (12, 'PHID-DCHG-twelve', 8, NULL, 'other.c', NULL, 1, 1, NULL, NULL, NULL, 0, 0, \
               1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_hunk (id, changesetID, oldOffset, \
               oldLen, newOffset, newLen, dataType, dataEncoding, dataFormat, data, dateCreated, \
               dateModified) VALUES \
             (21, 10, 9, 2, 9, 2, 'text', 'utf8', 'byte', ' x\\n-y\\n+z\\n', 1, 1), \
             (20, 10, 1, 2, 1, 2, 'text', 'utf8', 'gzde', UNHEX('{DEFLATED_HEX}'), 1, 1), \
             (22, 10, 20, 1, 20, 1, 'file', 'utf8', 'byte', 'PHID-FILE-hunk', 1, 1); \
             INSERT INTO `{{ns}}_file`.file_storageblob (id, data, dateCreated, dateModified) \
             VALUES (5, '-q\\n+r\\n', 1, 1)",
        ))
        .await;
    Some(test_db)
}

async fn insert_file(test_db: &TestDatabase, engine: &str, format: &str) {
    test_db
        .execute(&format!(
            "DELETE FROM `{{ns}}_file`.file; \
             INSERT INTO `{{ns}}_file`.file (phid, name, byteSize, storageEngine, \
               storageFormat, storageHandle, dateCreated, dateModified, metadata, mailKey, \
               viewPolicy) VALUES \
             ('PHID-FILE-hunk', 'differential-hunk', 6, '{engine}', '{format}', '5', 1, 1, \
               '{{}}', 'aaaaaaaaaaaaaaaaaaaa', 'no-one')"
        ))
        .await;
}

#[tokio::test]
async fn diffs_and_changesets_load() {
    let Some(test_db) = setup().await else {
        return;
    };
    insert_file(&test_db, "blob", "raw").await;
    let database = test_db.database();

    let diff = storage::load_diff(database, 7).await.unwrap().unwrap();
    assert_eq!(diff.phid, "PHID-DIFF-seven");
    assert_eq!(diff.revision_id, Some(3));
    assert_eq!(diff.base_revision.as_deref(), Some("abc123"));
    assert!(storage::load_diff(database, 8).await.unwrap().is_none());

    let changesets = storage::load_changesets(database, 7).await.unwrap();
    // Newest first, like DifferentialChangesetQuery.
    let ids: Vec<i64> = changesets.iter().map(|changeset| changeset.id).collect();
    assert_eq!(ids, [11, 10]);

    let moved = &changesets[0];
    assert_eq!(moved.change_type, ChangeType::MoveHere);
    assert_eq!(moved.old_file.as_deref(), Some("old.c"));
    assert_eq!(moved.hunks, []);

    let changed = &changesets[1];
    assert_eq!(changed.file_type, FileType::Text);
    assert_eq!(changed.metadata["line:first"], 2);
    // Hunks in ID order, from deflated, plain and file-backed data.
    let bodies: Vec<&str> = changed
        .hunks
        .iter()
        .map(|hunk| hunk.changes.as_str())
        .collect();
    assert_eq!(bodies, [" a\n-b\n+c\n", " x\n-y\n+z\n", "-q\n+r\n"]);
    assert_eq!(
        (changed.hunks[0].old_offset, changed.hunks[1].old_offset),
        (1, 9)
    );
    assert_eq!(changed.make_old_file(), "a\nb\nx\ny\nq\n");
    assert_eq!(changed.make_new_file(), "a\nc\nx\nz\nr\n");

    test_db.drop_databases().await;
}

#[tokio::test]
async fn unreadable_hunk_data_is_an_error() {
    let Some(test_db) = setup().await else {
        return;
    };
    insert_file(&test_db, "amazon-s3", "raw").await;
    let error = storage::load_changesets(test_db.database(), 7)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StorageError::File { hunk: 22, .. }),
        "{error}"
    );

    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_hunk SET dataType = 'text', \
             dataEncoding = NULL, data = 'x' WHERE id = 22",
        )
        .await;
    let error = storage::load_changesets(test_db.database(), 7)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StorageError::Encoding { hunk: 22, .. }),
        "{error}"
    );
    test_db.drop_databases().await;
}
