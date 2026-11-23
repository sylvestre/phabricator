//! The Git import pipeline against a throwaway `MySQL` database and the
//! working copies `scripts/seed-dev-git-repo.sh` builds: `bin/repository
//! update` of an observed repository (pull, discovery, refs), then the
//! message parser, change parser and publish tasks it queues, with the
//! rows PHP's engines and workers write (derived from the PHP code, see
//! each test).
//!
//! The observed repository R3 starts as an empty bare repository whose
//! remote is R2's working copy, as on the dev stack.
//!
//! Publishing a commit which isn't held needs a MIME command for its patch
//! (`file --brief --mime`, which the dev stack's `PHAB_MIME_COMMAND` is);
//! the tests which publish are skipped without `file`.

use std::path::{Path, PathBuf};
use std::process::Command;

use phab_app_diffusion::import::repository::ImportRepository;
use phab_app_diffusion::import::{ImportError, update};
use phab_app_files::mime::MimeCommand;
use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::{Database, rows};
use phab_worker::handlers::repository::{ChangeParserWorker, MessageParserWorker, PublishWorker};
use phab_worker::{Executed, LeaseOwner, Registry, Worker};
use serde_json::{Map, Value, json};

/// The commits of the seed script.
const C1: &str = "d61f24fe3e14bb3307532a9bc02968d7462cb336";
const C2: &str = "85948bd7ad4beca2df6fabad0d7f08b90365eff5";
const C3: &str = "59965686ade70d003a571d2352862b01b4c3c3b8";
const C4: &str = "20fb56cd8ff000f2f791dc647906b4c5a9bef267";
const C5: &str = "81bcc2f65c2b5d5a68333fc3df1f7dfe77482aa4";

const REPOSITORY: &str = "PHID-REPO-fab";
const MESSAGE: &str = "PhabricatorRepositoryGitCommitMessageParserWorker";
const CHANGE: &str = "PhabricatorRepositoryGitCommitChangeParserWorker";
const PUBLISH: &str = "PhabricatorRepositoryCommitPublishWorker";
const SEARCH: &str = "PhabricatorSearchWorker";
const UPDATE: &str = "DiffusionUpdateObjectAfterCommitWorker";
const TRANSACTION_PUBLISH: &str = "PhabricatorApplicationTransactionPublishWorker";
/// The HMAC key of file integrity hashes, which PHP makes.
const INTEGRITY_KEY: &str = "00112233445566778899aabbccddeeff";
/// The databases the pipeline reads and writes.
const DATABASES: &[&str] = &[
    "repository",
    "worker",
    "user",
    "herald",
    "file",
    "differential",
    "audit",
    "auth",
    "harbormaster",
];

type Row = Map<String, Value>;

struct Fixture {
    test_db: TestDatabase,
    directory: tempfile::TempDir,
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .env("GIT_AUTHOR_NAME", "bob")
        .env("GIT_AUTHOR_EMAIL", "bob@example.com")
        .env("GIT_COMMITTER_NAME", "bob")
        .env("GIT_COMMITTER_EMAIL", "bob@example.com")
        .env("GIT_AUTHOR_DATE", "1794600500 +0000")
        .env("GIT_COMMITTER_DATE", "1794600500 +0000")
        .output()
        .expect("running git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn fixture(details: &str) -> Option<Fixture> {
    let test_db = TestDatabase::create(DATABASES).await?;
    let directory = tempfile::tempdir().expect("a temporary directory");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/seed-dev-git-repo.sh");
    let origin = directory.path().join("2");
    let output = Command::new("sh")
        .arg(script)
        .arg(&origin)
        .output()
        .expect("running seed-dev-git-repo.sh");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed = directory.path().join("3");
    std::fs::create_dir(&observed).unwrap();
    git(&observed, &["init", "--quiet", "--bare"]);
    let details = details.replace("{origin}", &origin.display().to_string());
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             ('PHID-USER-alice', 'alice-phab', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, \
               isPrimary, dateCreated, dateModified) VALUES \
             ('PHID-EADR-alice', 'PHID-USER-alice', 'alice@example.com', 1, 1, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, \
               dateModified) VALUES ('file.integrity', '{INTEGRITY_KEY}', 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy, localPath) VALUES \
             (3, '{REPOSITORY}', 'Fabrique', 'FAB', 'git', '{details}', 1, 1, 'users', \
               'admin', 'users', '{}/')",
            observed.display()
        ))
        .await;
    Some(Fixture { test_db, directory })
}

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    fn origin(&self) -> PathBuf {
        self.directory.path().join("2")
    }

    async fn repository(&self) -> ImportRepository {
        ImportRepository::find(self.database(), "FAB")
            .await
            .unwrap()
            .unwrap()
    }

    async fn update(&self) -> Result<update::Outcome, ImportError> {
        let mut repository = self.repository().await;
        update::update(self.database(), &Config::default(), &mut repository, false).await
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

    /// One column of every row, as text (`NULL` for null).
    async fn column(&self, sql: &str) -> Vec<String> {
        self.rows(sql)
            .await
            .into_iter()
            .map(|row| {
                row.values()
                    .map(|value| value.as_str().unwrap_or("NULL").to_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    /// The commits, as `identifier epoch importStatus`.
    async fn commits(&self) -> Vec<String> {
        self.column(
            "SELECT CAST(commitIdentifier AS CHAR) c, CAST(epoch AS CHAR) e, \
               CAST(importStatus AS CHAR) s FROM `{ns}_repository`.repository_commit ORDER BY id",
        )
        .await
    }

    async fn commit_phid(&self, identifier: &str) -> String {
        self.column(&format!(
            "SELECT CAST(phid AS CHAR) FROM `{{ns}}_repository`.repository_commit \
             WHERE commitIdentifier = '{identifier}'"
        ))
        .await
        .remove(0)
    }

    /// The active tasks of `class`, as `priority objectPHID containerPHID
    /// data`.
    async fn tasks(&self, class: &str) -> Vec<String> {
        self.column(&format!(
            "SELECT CAST(task.priority AS CHAR) p, CAST(task.objectPHID AS CHAR) o, \
               CAST(task.containerPHID AS CHAR) c, CAST(data.data AS CHAR) d \
             FROM `{{ns}}_worker`.worker_activetask task JOIN `{{ns}}_worker`.worker_taskdata \
               data ON data.id = task.dataID WHERE task.taskClass = '{class}' ORDER BY task.id"
        ))
        .await
    }

    /// A worker without a MIME command: publishing is left to PHP.
    fn worker(&self) -> Worker {
        self.worker_with(Config::default(), PublishWorker::default())
    }

    fn worker_with(&self, config: Config, publish: PublishWorker) -> Worker {
        let mut registry = Registry::default();
        registry.register(MessageParserWorker);
        registry.register(ChangeParserWorker);
        registry.register(publish);
        Worker::new(
            self.database().clone(),
            config,
            registry,
            LeaseOwner::new("rust-test"),
        )
    }

    /// Runs the ported tasks until none is left; returns their classes.
    async fn run_all(&self) -> Vec<String> {
        run_all_with(&self.worker()).await
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

/// Runs the tasks `worker` takes until none is left; returns their classes.
async fn run_all_with(worker: &Worker) -> Vec<String> {
    let mut ran = Vec::new();
    while let Some(task) = worker.work_on(None).await.unwrap() {
        assert!(
            matches!(task.outcome, Ok(Executed::Succeeded(_))),
            "{} {:?}",
            task.class,
            task.outcome
        );
        ran.push(task.class);
    }
    ran
}

const IMPORTING: &str = r#"{"remote-uri":"{origin}","importing":true}"#;

/// `bin/repository update` of the empty observed repository: the pull
/// fetches every ref, discovery records the 5 commits in the order of
/// `reduceGraph()` (C5's ancestors, most ancestral first; C3 is the merge's
/// second parent, so it comes after C4), the ref engine creates a cursor
/// per ref, and the status messages say it went well.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn update_imports_an_observed_repository() {
    let Some(fixture) = fixture(IMPORTING).await else {
        return;
    };
    let outcome = fixture.update().await.unwrap();
    assert_eq!(outcome.output, ["Updated repository \"rFAB\"."]);
    // Every branch is permanent without a filter.
    assert_eq!(
        fixture.commits().await,
        [
            format!("{C1} 1794600000 1024"),
            format!("{C2} 1794600100 1024"),
            format!("{C4} 1794600300 1024"),
            format!("{C3} 1794600200 1024"),
            format!("{C5} 1794600400 1024"),
        ]
    );
    let other = fixture
        .column(
            "SELECT CAST(repositoryID AS CHAR) r, CAST(auditStatus AS CHAR) a, \
               CAST(summary AS CHAR) s, CAST(authorPHID AS CHAR) p, \
               CAST(authorIdentityPHID AS CHAR) i, CAST(committerIdentityPHID AS CHAR) c \
             FROM `{ns}_repository`.repository_commit",
        )
        .await;
    assert!(
        other.iter().all(|row| row == "3 none  NULL NULL NULL"),
        "{other:?}"
    );
    // C1 has no parent (an explicit 0), the merge two, in order.
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(childCommitID AS CHAR) c, CAST(parentCommitID AS CHAR) p FROM \
                 `{ns}_repository`.repository_parents ORDER BY id"
            )
            .await,
        ["1 0", "2 1", "3 2", "4 2", "5 3", "5 4"]
    );
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(commitID AS CHAR) c, CAST(authorName AS CHAR) a, \
                 CAST(commitMessage AS CHAR) m, CAST(commitDetails AS CHAR) d FROM \
                 `{ns}_repository`.repository_commitdata ORDER BY id"
            )
            .await,
        ["1   []", "2   []", "3   []", "4   []", "5   []"]
    );
    // The newest commit by epoch is the last.
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(repositoryID AS CHAR) r, CAST(size AS CHAR) s, \
                 CAST(lastCommitID AS CHAR) l, CAST(epoch AS CHAR) e FROM \
                 `{ns}_repository`.repository_summary"
            )
            .await,
        ["3 5 5 1794600400"]
    );
    // An importing repository imports at PRIORITY_IMPORT.
    let mut expected = Vec::new();
    for identifier in [C1, C2, C4, C3, C5] {
        let phid = fixture.commit_phid(identifier).await;
        expected.push(format!(
            "4000 {phid} {REPOSITORY} {{\"commitPHID\":\"{phid}\",\"via\":\"discovery\"}}"
        ));
    }
    assert_eq!(fixture.tasks(MESSAGE).await, expected);
    // A cursor per ref, in the order of for-each-ref (newest first) by
    // type; only branches are permanent.
    let cursors = fixture
        .rows(
            "SELECT CAST(c.refType AS CHAR) t, CAST(c.refNameRaw AS CHAR) n, \
               CAST(c.refNameHash AS CHAR) h, CAST(c.refNameEncoding AS CHAR) e, \
               CAST(c.isPermanent AS CHAR) p, CAST(p.commitIdentifier AS CHAR) i, \
               CAST(p.isClosed AS CHAR) x, CAST(c.phid AS CHAR) phid \
             FROM `{ns}_repository`.repository_refcursor c JOIN \
               `{ns}_repository`.repository_refposition p ON p.cursorID = c.id ORDER BY c.id",
        )
        .await;
    let summary: Vec<String> = cursors
        .iter()
        .map(|row| {
            assert_eq!(
                row["h"].as_str().unwrap(),
                phab_core::hash::digest_for_index(row["n"].as_str().unwrap().as_bytes())
            );
            assert!(row["phid"].as_str().unwrap().starts_with("PHID-RREF-"));
            format!(
                "{} {} {} {} {} {}",
                row["t"].as_str().unwrap(),
                row["n"].as_str().unwrap(),
                row["e"].as_str().unwrap(),
                row["p"].as_str().unwrap(),
                row["i"].as_str().unwrap(),
                row["x"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            format!("branch master utf8 1 {C5} 0"),
            format!("branch feature utf8 1 {C3} 0"),
            format!("tag v2 utf8 0 {C2} 0"),
            format!("tag v1 utf8 0 {C1} 0"),
        ]
    );
    // pullRepository(), then the update: needs-update deleted, fetch okay.
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(statusType AS CHAR) t, CAST(statusCode AS CHAR) c, \
                 CAST(parameters AS CHAR) p, CAST(messageCount AS CHAR) n FROM \
                 `{ns}_repository`.repository_statusmessage ORDER BY id"
            )
            .await,
        ["init okay {\"message\":null} 1", "fetch okay [] 1"]
    );
    // Still importing: no commit is fully imported.
    assert!(fixture.repository().await.is_importing());
    // The working copy has the remote's refs, and HEAD on master.
    let observed = fixture.directory.path().join("3");
    assert_eq!(git(&observed, &["rev-parse", "refs/heads/feature"]), C3);
    assert_eq!(
        git(&observed, &["symbolic-ref", "HEAD"]),
        "refs/heads/master"
    );

    // A second update finds nothing new: same refs, nothing fetched, no
    // new task, the status counts go up.
    fixture.update().await.unwrap();
    assert_eq!(fixture.tasks(MESSAGE).await.len(), 5);
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(messageCount AS CHAR) n FROM \
                 `{ns}_repository`.repository_statusmessage ORDER BY id"
            )
            .await,
        ["2", "2"]
    );
    fixture.finish().await;
}

/// The tasks discovery queues, run in queue order (`PRIORITY_DEFAULT`
/// follow-ups first): each commit gets its message, its changes, a search
/// task, and its hold reasons (the repository is importing), so every
/// commit ends at `IMPORTED_ALL | IMPORTED_PERMANENT`.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn parses_every_commit() {
    let Some(fixture) = fixture(IMPORTING).await else {
        return;
    };
    fixture.update().await.unwrap();
    let ran = fixture.run_all().await;
    assert_eq!(ran.len(), 15);
    assert_eq!(ran[..3], [MESSAGE, CHANGE, PUBLISH]);
    assert!(
        fixture
            .commits()
            .await
            .iter()
            .all(|row| row.ends_with(" 1035"))
    );

    // One identity for alice, first named by C1, resolved by address.
    let c1 = fixture.commit_phid(C1).await;
    let identities = fixture
        .rows(
            "SELECT CAST(phid AS CHAR) phid, CAST(identityNameRaw AS CHAR) n, \
               CAST(identityNameHash AS CHAR) h, CAST(identityNameEncoding AS CHAR) e, \
               CAST(automaticGuessedUserPHID AS CHAR) a, CAST(manuallySetUserPHID AS CHAR) m, \
               CAST(currentEffectiveUserPHID AS CHAR) u, CAST(authorPHID AS CHAR) s, \
               CAST(emailAddress AS CHAR) x FROM `{ns}_repository`.repository_identity",
        )
        .await;
    assert_eq!(identities.len(), 1);
    let identity = &identities[0];
    let identity_phid = identity["phid"].as_str().unwrap().to_owned();
    assert!(identity_phid.starts_with("PHID-RIDT-"));
    assert_eq!(identity["n"], json!("alice <alice@example.com>"));
    assert_eq!(
        identity["h"].as_str().unwrap(),
        phab_core::hash::digest_for_index(b"alice <alice@example.com>")
    );
    assert_eq!(identity["e"], json!("utf8"));
    assert_eq!(identity["a"], json!("PHID-USER-alice"));
    assert_eq!(identity["m"], Value::Null);
    assert_eq!(identity["u"], json!("PHID-USER-alice"));
    assert_eq!(identity["s"], json!(c1));
    assert_eq!(identity["x"], json!("alice@example.com"));

    let commit = fixture
        .column(&format!(
            "SELECT CAST(summary AS CHAR) s, CAST(authorPHID AS CHAR) a, \
               CAST(authorIdentityPHID AS CHAR) i, CAST(committerIdentityPHID AS CHAR) c \
             FROM `{{ns}}_repository`.repository_commit WHERE phid = '{c1}'"
        ))
        .await;
    assert_eq!(
        commit,
        [format!(
            "Add the README PHID-USER-alice {identity_phid} {identity_phid}"
        )]
    );
    let data = fixture
        .rows(
            "SELECT CAST(authorName AS CHAR) a, CAST(commitMessage AS CHAR) m, \
               CAST(commitDetails AS CHAR) d FROM `{ns}_repository`.repository_commitdata \
             WHERE commitID = 1",
        )
        .await
        .remove(0);
    assert_eq!(data["a"], json!("alice <alice@example.com>"));
    assert_eq!(data["m"], json!("Add the README\n"));
    let tree = git(&fixture.origin(), &["rev-parse", &format!("{C1}^{{tree}}")]);
    // json_encode(): "/" escaped; the keys in the order PHP sets them.
    let expected = format!(
        "{{\"ref\":{{\"authorEpoch\":1794600000,\"authorName\":\"alice\",\
         \"authorEmail\":\"alice@example.com\",\"committerName\":\"alice\",\
         \"committerEmail\":\"alice@example.com\",\"message\":\"Add the README\\n\",\
         \"hashes\":[{{\"type\":\"gtcm\",\"value\":\"{C1}\"}},{{\"type\":\"gttr\",\
         \"value\":\"{tree}\"}}]}},\"authorEpoch\":1794600000,\"authorName\":\"alice\",\
         \"authorEmail\":\"alice@example.com\",\"authorIdentityPHID\":\"{identity_phid}\",\
         \"authorPHID\":\"PHID-USER-alice\",\"committer\":\"alice <alice@example.com>\",\
         \"committerName\":\"alice\",\"committerEmail\":\"alice@example.com\",\
         \"committerPHID\":\"PHID-USER-alice\",\"committerIdentityPHID\":\"{identity_phid}\",\
         \"holdReasons\":[\"auto\\/importing\"]}}"
    );
    assert_eq!(data["d"].as_str().unwrap(), expected);

    // The changes of C2 against C1: README changed, dir and its file added
    // (`-t` lists trees), a symlink added, and the "/" PHP adds.
    let changes = fixture
        .column(
            "SELECT CAST(p.path AS CHAR) path, CAST(c.repositoryID AS CHAR) r, \
               CAST(c.changeType AS CHAR) t, CAST(c.fileType AS CHAR) f, \
               CAST(c.isDirect AS CHAR) d, CAST(c.commitSequence AS CHAR) s, \
               CAST(c.targetPathID AS CHAR) tp, CAST(c.targetCommitID AS CHAR) tc \
             FROM `{ns}_repository`.repository_pathchange c JOIN \
               `{ns}_repository`.repository_path p ON p.id = c.pathID WHERE c.commitID = 2 \
             ORDER BY p.path",
        )
        .await;
    assert_eq!(
        changes,
        [
            "/ 3 10 4 0 1794600100 NULL NULL",
            "/README 3 2 7 1 1794600100 NULL NULL",
            "/dir 3 1 4 1 1794600100 NULL NULL",
            "/dir/a.txt 3 1 7 1 1794600100 NULL NULL",
            "/link 3 1 5 1 1794600100 NULL NULL",
        ]
    );
    // C4 deletes a.txt, and so dir (b.txt is on feature): a deleted tree
    // has mode 000000, which PHP reads as a normal file.
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(p.path AS CHAR) path, CAST(c.changeType AS CHAR) t, \
                   CAST(c.fileType AS CHAR) f, CAST(c.isDirect AS CHAR) d FROM \
                   `{ns}_repository`.repository_pathchange c JOIN \
                   `{ns}_repository`.repository_path p ON p.id = c.pathID WHERE c.commitID = 3 \
                 ORDER BY p.path"
            )
            .await,
        ["/ 10 4 0", "/dir 3 7 1", "/dir/a.txt 3 7 1"]
    );
    // Paths are shared, each with its md5.
    let paths = fixture
        .column(
            "SELECT CAST(path AS CHAR) p, CAST(pathHash AS CHAR) h FROM \
             `{ns}_repository`.repository_path ORDER BY id",
        )
        .await;
    assert_eq!(paths.len(), 6, "{paths:?}");
    assert!(
        paths
            .iter()
            .any(|row| row == "/ 6666cd76f96956469e7be39d750cc7d9")
    );
    // A search task per commit, at PRIORITY_INDEX.
    assert_eq!(
        fixture.tasks(SEARCH).await[0],
        format!("3500 {c1} NULL {{\"documentPHID\":\"{c1}\",\"parameters\":[],\"strict\":false}}")
    );
    assert_eq!(fixture.tasks(SEARCH).await.len(), 5);
    fixture.finish().await;
}

/// The follow-ups of the message parser keep `via` and are queued at
/// `PRIORITY_DEFAULT` with the commit and its repository; a step already
/// done is skipped (still queuing the next one).
#[tokio::test]
async fn queues_the_next_step_and_skips_done_ones() {
    let Some(fixture) = fixture(IMPORTING).await else {
        return;
    };
    fixture.update().await.unwrap();
    fixture
        .test_db
        .execute(
            "UPDATE `{ns}_repository`.repository_commit SET importStatus = importStatus | 1 \
             WHERE id = 1",
        )
        .await;
    let worker = fixture.worker();
    let ran = worker.work_on(None).await.unwrap().unwrap();
    assert_eq!(ran.class, MESSAGE);
    assert!(matches!(ran.outcome, Ok(Executed::Succeeded(_))));
    let c1 = fixture.commit_phid(C1).await;
    assert_eq!(
        fixture.tasks(CHANGE).await,
        [format!(
            "2000 {c1} {REPOSITORY} {{\"commitPHID\":\"{c1}\",\"via\":\"discovery\"}}"
        )]
    );
    // Skipped: no identity, the data untouched.
    assert_eq!(
        fixture
            .column("SELECT phid FROM `{ns}_repository`.repository_identity")
            .await,
        Vec::<String>::new()
    );
    fixture.finish().await;
}

/// A branch deleted in the remote: the pull deletes the local ref, the ref
/// engine records its position as an old ref, and the next discovery marks
/// the commits only it had unreachable and rebuilds the summary.
#[tokio::test]
async fn marks_the_commits_of_deleted_branches_unreachable() {
    let Some(fixture) = fixture(IMPORTING).await else {
        return;
    };
    let origin = fixture.origin();
    git(&origin, &["checkout", "--quiet", "-b", "topic"]);
    git(
        &origin,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            "Topic",
        ],
    );
    let topic = git(&origin, &["rev-parse", "HEAD"]);
    git(&origin, &["checkout", "--quiet", "master"]);
    fixture.update().await.unwrap();
    assert_eq!(fixture.commits().await.len(), 6);

    git(&origin, &["branch", "--quiet", "-D", "topic"]);
    fixture.update().await.unwrap();
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(commitIdentifier AS CHAR) FROM \
                 `{ns}_repository`.repository_oldref"
            )
            .await,
        std::slice::from_ref(&topic)
    );
    let observed = fixture.directory.path().join("3");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/topic"])
        .current_dir(&observed)
        .output()
        .unwrap();
    assert!(!output.status.success(), "the local ref is gone");
    fixture.update().await.unwrap();
    assert_eq!(
        fixture
            .column("SELECT id FROM `{ns}_repository`.repository_oldref")
            .await,
        Vec::<String>::new()
    );
    let commits = fixture.commits().await;
    assert_eq!(commits[5], format!("{topic} 1794600500 3072"));
    assert!(commits[..5].iter().all(|row| row.ends_with(" 1024")));
    // rebuildSummaryTable(): the reachable commits only.
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(size AS CHAR) s, CAST(lastCommitID AS CHAR) l, \
                 CAST(epoch AS CHAR) e FROM `{ns}_repository`.repository_summary"
            )
            .await,
        ["5 5 1794600400"]
    );
    // Its import task fails permanently, as PHP's does.
    let worker = fixture.worker();
    let mut failed = false;
    while let Some(task) = worker.work_on(None).await.unwrap() {
        if let Ok(Executed::FailedPermanently { message, .. }) = &task.outcome {
            assert!(message.contains("is no longer reachable"), "{message}");
            failed = true;
        }
    }
    assert!(failed);
    fixture.finish().await;
}

/// What the pipeline leaves to PHP, before writing anything: a hosted
/// repository's update (PHP installs its hooks), a repository with a
/// branch filter which is a regular expression, Mercurial, and the
/// publishing of a commit the publisher doesn't hold without a MIME command.
#[tokio::test]
async fn leaves_what_is_not_ported_to_php() {
    let Some(fixture) = fixture(r#"{"remote-uri":"{origin}","hosting-enabled":true}"#).await else {
        return;
    };
    assert!(matches!(
        fixture.update().await,
        Err(ImportError::Unsupported(_))
    ));
    // Discovery itself works on hosted repositories.
    let mut repository = fixture.repository().await;
    let observed = fixture.directory.path().join("3");
    git(
        &observed,
        &[
            "fetch",
            "--quiet",
            &fixture.origin().display().to_string(),
            "+refs/*:refs/*",
        ],
    );
    update::discover(
        fixture.database(),
        &Config::default(),
        &mut repository,
        false,
    )
    .await
    .unwrap();
    assert_eq!(fixture.commits().await.len(), 5);
    // Not importing (5 commits are no import), not held: published by PHP
    // without a MIME command for the patches.
    let worker = fixture.worker();
    let mut classes = Vec::new();
    while let Some(task) = worker.work_on(None).await.unwrap() {
        classes.push(task.class);
    }
    assert_eq!(classes.len(), 10);
    assert_eq!(fixture.tasks(PUBLISH).await.len(), 5);
    assert!(
        fixture
            .commits()
            .await
            .iter()
            .all(|row| row.ends_with(" 1027"))
    );

    fixture
        .test_db
        .execute(
            "UPDATE `{ns}_repository`.repository SET details = \
             '{\"branch-filter\":{\"regexp(/^m/)\":true}}' WHERE id = 3",
        )
        .await;
    let mut repository = fixture.repository().await;
    assert!(matches!(
        update::discover(
            fixture.database(),
            &Config::default(),
            &mut repository,
            false
        )
        .await,
        Err(ImportError::Unsupported(_))
    ));
    fixture
        .test_db
        .execute("UPDATE `{ns}_repository`.repository SET versionControlSystem = 'hg'")
        .await;
    let mut repository = fixture.repository().await;
    assert!(matches!(
        update::discover(
            fixture.database(),
            &Config::default(),
            &mut repository,
            false
        )
        .await,
        Err(ImportError::Unsupported(_))
    ));
    fixture.finish().await;
}

/// `file --brief --mime`, if this host has `file`.
fn mime_command() -> Option<MimeCommand> {
    let found = Command::new("file")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !found {
        eprintln!("skipping: no file command");
        return None;
    }
    MimeCommand::parse("file --brief --mime")
}

/// The configuration of the publishing tests: an install at
/// `https://phab.test/`.
fn publish_config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "https://phab.test/"}) {
        config.push_layer("test", map);
    }
    config
}

/// D7, an accepted revision of R3.
const REVISION: &str = "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, \
     status, summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
     viewPolicy, editPolicy, properties, activeDiffPHID, repositoryPHID) VALUES (7, 'Fix', \
     'PHID-DREV-seven', 'accepted', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', 'users', 'users', \
     '{}', '', 'PHID-REPO-fab')";

/// Adds a commit changing `c.txt` on the origin's master; returns its hash.
fn commit_on_origin(fixture: &Fixture, message: &str) -> String {
    let origin = fixture.origin();
    std::fs::write(origin.join("c.txt"), "sea\n").unwrap();
    git(&origin, &["add", "c.txt"]);
    git(
        &origin,
        &["commit", "--quiet", "--no-gpg-sign", "-m", message],
    );
    git(&origin, &["rev-parse", "HEAD"])
}

/// A repository which publishes (6 commits are no import): each commit gets
/// its hold reasons cleared, its patch as a temporary file attached to the
/// repository, the `audit:commit` transaction acting as its committer, a
/// Herald transcript without rules, the "none" header, and the transaction
/// publish task with the editor's state. The commit naming D7, a revision
/// of its repository, records it and queues
/// `DiffusionUpdateObjectAfterCommitWorker`, after the step.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn publishes_commits_and_closes_the_revision_they_name() {
    let Some(mime) = mime_command() else {
        return;
    };
    let Some(fixture) = fixture(r#"{"remote-uri":"{origin}"}"#).await else {
        return;
    };
    fixture.test_db.execute(REVISION).await;
    let fix = commit_on_origin(
        &fixture,
        "Fix the README\n\nDifferential Revision: https://phab.test/D7",
    );
    fixture.update().await.unwrap();
    let worker = fixture.worker_with(publish_config(), PublishWorker::with_mime(Some(mime)));
    let ran = run_all_with(&worker).await;
    assert_eq!(ran.len(), 18);
    assert!(
        fixture
            .commits()
            .await
            .iter()
            .all(|row| row.ends_with(" 1035"))
    );
    let commit = fixture.commit_phid(&fix).await;
    let identity = fixture
        .column(
            "SELECT CAST(phid AS CHAR) FROM `{ns}_repository`.repository_identity WHERE \
             identityNameRaw = 'bob <bob@example.com>'",
        )
        .await
        .remove(0);

    // closeRevisions(): the details, after the hold reasons.
    let details = fixture
        .column(&format!(
            "SELECT CAST(d.commitDetails AS CHAR) FROM `{{ns}}_repository`.repository_commitdata \
             d JOIN `{{ns}}_repository`.repository_commit c ON c.id = d.commitID WHERE \
             c.phid = '{commit}'"
        ))
        .await
        .remove(0);
    let match_data = "{\"usedURI\":true,\"foundURI\":7,\"validDomain\":\"https:\\/\\/phab.test\",\
                      \"matchHashType\":null,\"matchHashValue\":null}";
    assert!(
        details.ends_with(&format!(
            ",\"holdReasons\":[],\"differential.revisionID\":7,\"revisionMatchData\":{match_data}}}"
        )),
        "{details}"
    );
    // The other commits name no revision.
    assert_eq!(
        fixture
            .column(
                "SELECT CAST(id AS CHAR) FROM `{ns}_repository`.repository_commitdata WHERE commitDetails \
                 LIKE '%revisionMatchData%'"
            )
            .await
            .len(),
        1
    );
    assert_eq!(
        fixture.tasks(UPDATE).await,
        [format!(
            "2000 NULL NULL {{\"commitPHID\":\"{commit}\",\"objectPHID\":\"PHID-DREV-seven\",\
             \"properties\":{{\"revisionMatchData\":{match_data}}}}}"
        )]
    );

    // The transaction, acting as bob's identity (no user), dated as the
    // commit.
    let xactions = fixture
        .rows(&format!(
            "SELECT CAST(phid AS CHAR) phid, CAST(authorPHID AS CHAR) a, \
               CAST(viewPolicy AS CHAR) v, CAST(editPolicy AS CHAR) e, \
               CAST(transactionType AS CHAR) t, CAST(oldValue AS CHAR) o, \
               CAST(newValue AS CHAR) n, CAST(contentSource AS CHAR) s, \
               CAST(metadata AS CHAR) m, CAST(dateCreated AS CHAR) d \
             FROM `{{ns}}_audit`.audit_transaction WHERE objectPHID = '{commit}'"
        ))
        .await;
    assert_eq!(xactions.len(), 1);
    let xaction = &xactions[0];
    let xaction_phid = xaction["phid"].as_str().unwrap().to_owned();
    assert!(xaction_phid.starts_with("PHID-XACT-CMIT-"));
    assert_eq!(xaction["a"], json!(identity));
    assert_eq!(xaction["v"], json!("public"));
    assert_eq!(xaction["e"], json!("no-one"));
    assert_eq!(xaction["t"], json!("audit:commit"));
    assert_eq!(xaction["o"], json!("null"));
    assert_eq!(
        xaction["n"],
        json!(
            "{\"description\":\"Fix the README\\n\\nDifferential Revision: \
             https:\\/\\/phab.test\\/D7\\n\",\"summary\":\"Fix the README\",\"authorName\":\
             \"bob <bob@example.com>\",\"authorPHID\":null,\"committerName\":\
             \"bob <bob@example.com>\",\"committerPHID\":null}"
        )
    );
    assert_eq!(xaction["s"], json!("{\"source\":\"daemon\",\"params\":[]}"));
    let metadata: Value = serde_json::from_str(xaction["m"].as_str().unwrap()).unwrap();
    assert_eq!(metadata["core.groupID"].as_str().unwrap().len(), 32);
    assert_eq!(xaction["d"], json!("1794600500"));
    assert_eq!(
        fixture
            .column("SELECT CAST(id AS CHAR) FROM `{ns}_audit`.audit_transaction")
            .await
            .len(),
        6
    );

    // Herald, without rules.
    let transcripts = fixture
        .rows(&format!(
            "SELECT CAST(objectTranscript AS CHAR) o, CAST(applyTranscripts AS CHAR) a \
             FROM `{{ns}}_herald`.herald_transcript WHERE objectPHID = '{commit}'"
        ))
        .await;
    assert_eq!(transcripts.len(), 1);
    let object = transcripts[0]["o"].as_str().unwrap();
    assert!(object.contains(&format!("\"rFAB{fix}\"")), "{object}");
    assert!(object.contains(&format!("\"{xaction_phid}\"")), "{object}");
    assert!(object.contains("s:6:\"commit\""), "{object}");
    assert_eq!(transcripts[0]["a"], json!("a:0:{}"));
    assert_eq!(
        fixture
            .column(&format!(
                "SELECT CAST(header AS CHAR) FROM `{{ns}}_herald`.herald_savedheader WHERE \
                 phid = '{commit}'"
            ))
            .await,
        ["none"]
    );

    // The patch: a file no one sees for 48 hours, attached to R3.
    let files = fixture
        .rows(
            "SELECT CAST(f.phid AS CHAR) phid, CAST(f.name AS CHAR) n, \
               CAST(f.mimeType AS CHAR) m, CAST(f.viewPolicy AS CHAR) v, \
               CAST(f.ttl - f.dateCreated AS CHAR) t, CAST(f.isExplicitUpload AS CHAR) x, \
               CAST(f.storageEngine AS CHAR) e, CAST(b.data AS CHAR) d \
             FROM `{ns}_file`.file f JOIN `{ns}_file`.file_storageblob b ON \
               b.id = f.storageHandle ORDER BY f.id",
        )
        .await;
    assert_eq!(files.len(), 6);
    let patch = files
        .iter()
        .find(|file| file["d"].as_str().unwrap().contains("+++ b/c.txt"))
        .unwrap();
    assert_eq!(patch["n"], json!(""));
    assert_eq!(patch["m"], json!("text/x-diff"));
    assert_eq!(patch["v"], json!("no-one"));
    assert_eq!(patch["t"], json!("172800"));
    assert_eq!(patch["x"], json!("0"));
    assert_eq!(patch["e"], json!("blob"));
    let file_phid = patch["phid"].as_str().unwrap();
    assert_eq!(
        fixture
            .column(&format!(
                "SELECT CAST(objectPHID AS CHAR) o, CAST(attachmentMode AS CHAR) m, \
                 CAST(attacherPHID AS CHAR) a FROM `{{ns}}_file`.file_attachment WHERE \
                 filePHID = '{file_phid}'"
            ))
            .await,
        [format!("{REPOSITORY} attach NULL")]
    );
    // C3 and the merge add the same file: one blob for both patches.
    assert_eq!(
        fixture
            .column("SELECT CAST(id AS CHAR) FROM `{ns}_file`.file_storageblob")
            .await
            .len(),
        5
    );
    // A search task per commit, then per file.
    assert_eq!(fixture.tasks(SEARCH).await.len(), 12);

    // The publish task, at PRIORITY_ALERTS.
    let published = fixture
        .rows(&format!(
            "SELECT CAST(task.priority AS CHAR) p, CAST(data.data AS CHAR) d FROM \
             `{{ns}}_worker`.worker_activetask task JOIN `{{ns}}_worker`.worker_taskdata data \
             ON data.id = task.dataID WHERE task.taskClass = '{TRANSACTION_PUBLISH}' AND \
             task.objectPHID = '{commit}'"
        ))
        .await;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["p"], json!("1000"));
    let data: Value = serde_json::from_str(published[0]["d"].as_str().unwrap()).unwrap();
    assert_eq!(data["objectPHID"], json!(commit));
    assert_eq!(data["actorPHID"], json!(identity));
    assert_eq!(data["xactionPHIDs"], json!([xaction_phid]));
    let state = &data["state"];
    assert_eq!(state["mailToPHIDs"], json!([identity]));
    assert_eq!(state["mailCCPHIDs"], json!([]));
    assert_eq!(state["feedNotifyPHIDs"], json!([identity]));
    assert_eq!(state["feedRelatedPHIDs"], json!([commit, identity]));
    assert_eq!(state["heraldHeader"], json!("none"));
    assert_eq!(state["custom"]["affectedFiles"], json!("/c.txt"));
    assert_eq!(state["custom"]["auditorPHIDs"], json!([]));
    assert_eq!(state["custom.encoding"], json!({"rawPatch": "binary"}));
    let raw = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(state["custom"]["rawPatch"].as_str().unwrap())
            .unwrap()
    };
    let patch_text = patch["d"].as_str().unwrap();
    assert_eq!(
        String::from_utf8(raw).unwrap(),
        format!("s:{}:\"{patch_text}\";", patch_text.len())
    );
    let stamps = state["mailStamps"].as_array().unwrap();
    assert_eq!(stamps[1]["value"], json!(format!("rFAB{fix}")));
    assert_eq!(
        stamps[5],
        json!({"type": "string", "key": "via", "value": "daemon"})
    );
    fixture.finish().await;
}

/// What publishing leaves to PHP before the lease: everything while an
/// enabled Herald rule on commits exists, then a commit mentioning a user.
#[tokio::test]
async fn leaves_publishing_with_herald_rules_or_mentions_to_php() {
    let Some(mime) = mime_command() else {
        return;
    };
    let Some(fixture) = fixture(r#"{"remote-uri":"{origin}"}"#).await else {
        return;
    };
    let thanks = commit_on_origin(&fixture, "Thanks @alice-phab");
    fixture.update().await.unwrap();
    fixture
        .test_db
        .execute(
            "INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, \
               ruleType, phid, isDisabled, triggerObjectPHID) VALUES \
             (1, 'Audit', 'PHID-USER-alice', 'commit', 1, 38, 1, 1, 'every', 'global', \
               'PHID-HRUL-1', 0, NULL)",
        )
        .await;
    let worker = fixture.worker_with(
        publish_config(),
        PublishWorker::with_mime(Some(mime.clone())),
    );
    assert_eq!(run_all_with(&worker).await.len(), 12);
    assert_eq!(fixture.tasks(PUBLISH).await.len(), 6);
    assert_eq!(
        fixture
            .column("SELECT CAST(id AS CHAR) FROM `{ns}_audit`.audit_transaction")
            .await,
        Vec::<String>::new()
    );

    fixture
        .test_db
        .execute("UPDATE `{ns}_herald`.herald_rule SET isDisabled = 1")
        .await;
    let worker = fixture.worker_with(publish_config(), PublishWorker::with_mime(Some(mime)));
    assert_eq!(run_all_with(&worker).await.len(), 5);
    let thanks = fixture.commit_phid(&thanks).await;
    let left = fixture.tasks(PUBLISH).await;
    assert_eq!(left.len(), 1);
    assert!(left[0].contains(&thanks), "{left:?}");
    assert_eq!(
        fixture
            .column(&format!(
                "SELECT CAST(id AS CHAR) FROM `{{ns}}_audit`.audit_transaction WHERE objectPHID = '{thanks}'"
            ))
            .await,
        Vec::<String>::new()
    );
    fixture.finish().await;
}
