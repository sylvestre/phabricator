//! The Diffusion pages against one test database and the working copy
//! `scripts/seed-dev-git-repo.sh` builds.
//!
//! GIT (R1) is the working copy, visible to users; SEC (R3) the same
//! working copy, visible to administrators only; HG (R2) a Mercurial
//! repository; R7 the working copy again, with the short name "slugged".
//! Commits C2 and C5 are imported and published; C3 is not on a permanent
//! ref, C1 needs an audit, C4 is not imported.

// The checks share one database, so each page's are one long function.
#![allow(clippy::too_many_lines)]

use std::path::Path;
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

const NOW: i64 = 1_794_700_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

const ALICE: &str = "phsid=alice-token";
const ROOT: &str = "phsid=root-token";

const C1: &str = "d61f24fe3e14bb3307532a9bc02968d7462cb336";
const C2: &str = "85948bd7ad4beca2df6fabad0d7f08b90365eff5";
const C3: &str = "59965686ade70d003a571d2352862b01b4c3c3b8";
const C4: &str = "20fb56cd8ff000f2f791dc647906b4c5a9bef267";
const C5: &str = "81bcc2f65c2b5d5a68333fc3df1f7dfe77482aa4";

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

/// The failed checks: the pages are slow to set up, so one run reports
/// every failure.
static FAILURES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn fail(message: String) {
    FAILURES.lock().unwrap().push(message);
}

macro_rules! check {
    ($condition:expr, $page:expr) => {
        if !$condition {
            fail(format!(
                "line {}: {}\n{}",
                line!(),
                stringify!($condition),
                $page
            ));
        }
    };
    ($condition:expr) => {
        if !$condition {
            fail(format!("line {}: {}", line!(), stringify!($condition)));
        }
    };
}

macro_rules! check_eq {
    ($left:expr, $right:expr) => {{
        let (left, right) = (&$left, &$right);
        if left != right {
            fail(format!(
                "line {}: {} == {}: {left:?} != {right:?}",
                line!(),
                stringify!($left),
                stringify!($right)
            ));
        }
    }};
}

struct Fixture {
    test_db: TestDatabase,
    _directory: tempfile::TempDir,
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

fn seed_repository(directory: &Path) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/seed-dev-git-repo.sh");
    let output = std::process::Command::new("sh")
        .arg(script)
        .arg(directory)
        .output()
        .expect("running seed-dev-git-repo.sh");
    assert!(
        output.status.success(),
        "seed-dev-git-repo.sh: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn setup() -> Option<Fixture> {
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
        "audit",
        "cache",
        "search",
        "daemon",
    ])
    .await?;
    let directory = tempfile::tempdir().expect("a temporary directory");
    let copy = directory.path().join("2");
    seed_repository(&copy);
    // localPath is unique: each repository spells the path its own way.
    let base = copy.display().to_string();
    let (git, hg, secret, slugged) = (
        format!("{base}/"),
        format!("{base}/."),
        format!("{base}/./"),
        format!("{base}/./."),
    );
    let master = hash::digest_for_index(b"master");
    let commit_value = serde_json::json!({
        "description": "Add a.txt and a link",
        "summary": "Add a.txt and a link",
        "authorName": "alice <alice@example.com>",
        "authorPHID": "PHID-USER-alice",
        "committerName": "",
        "committerPHID": null,
    })
    .to_string();
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-root', 'web', '{root}', 1, {expires}, 'PHID-SSSN-2', 0); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               repositorySlug, versionControlSystem, details, dateCreated, dateModified, \
               viewPolicy, editPolicy, pushPolicy, localPath) VALUES \
             (1, 'PHID-REPO-git', 'Git', 'GIT', NULL, 'git', \
               '{{\"tracking-enabled\":\"active\",\"description\":\"The **test** repository\"}}', \
               1, 1, 'users', 'admin', 'admin', '{git}'), \
             (2, 'PHID-REPO-hg', 'Hg', 'HG', NULL, 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin', \
               '{hg}'), \
             (3, 'PHID-REPO-secret', 'Secret', 'SEC', NULL, 'git', '{{}}', 1, 1, 'admin', 'admin', \
               'admin', '{secret}'), \
             (7, 'PHID-REPO-slugged', 'Slugged', NULL, 'slugged', 'git', '{{}}', 1, 1, 'users', \
               'admin', 'admin', '{slugged}'); \
             INSERT INTO `{{ns}}_repository`.repository_refcursor (id, phid, repositoryPHID, \
               refType, refNameHash, refNameRaw, refNameEncoding, isPermanent) VALUES \
             (1, 'PHID-RREF-1', 'PHID-REPO-git', 'branch', '{master}', 'master', 'utf8', 1), \
             (2, 'PHID-RREF-2', 'PHID-REPO-secret', 'branch', '{master}', 'master', 'utf8', 1), \
             (3, 'PHID-RREF-3', 'PHID-REPO-slugged', 'branch', '{master}', 'master', 'utf8', 1); \
             INSERT INTO `{{ns}}_repository`.repository_refposition (cursorID, commitIdentifier, \
               isClosed) VALUES (1, '{C5}', 0), (2, '{C5}', 0), (3, '{C5}', 0); \
             INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, summary, importStatus, authorPHID) VALUES \
             (10, 1, 'PHID-CMIT-2', '{C2}', 1794600100, 'none', 'Add a.txt and a link', 1035, \
               'PHID-USER-alice'), \
             (11, 1, 'PHID-CMIT-5', '{C5}', 1794600400, 'none', 'Merge branch ''feature''', 1035, \
               NULL), \
             (12, 1, 'PHID-CMIT-3', '{C3}', 1794600200, 'none', 'Add b.txt', 11, NULL), \
             (13, 1, 'PHID-CMIT-1', '{C1}', 1794600000, 'needs-audit', 'Add the README', 1035, \
               NULL); \
             INSERT INTO `{{ns}}_repository`.repository_commitdata (commitID, authorName, \
               commitMessage, commitDetails) VALUES \
             (10, 'alice <alice@example.com>', 'Add a.txt and a link\n\nSee D1.', \
               '{{\"authorPHID\":\"PHID-USER-alice\",\"authorEpoch\":1794600100}}'), \
             (11, 'alice <alice@example.com>', 'Merge branch ''feature''', '{{}}'), \
             (12, 'alice <alice@example.com>', 'Add b.txt', '{{}}'), \
             (13, 'alice <alice@example.com>', 'Add the README', '{{}}'); \
             INSERT INTO `{{ns}}_repository`.repository_path (id, path, pathHash) VALUES \
             (1, '/README', MD5('/README')), (2, '/dir/a.txt', MD5('/dir/a.txt')), \
             (3, '/link', MD5('/link')), (4, '/dir/b.txt', MD5('/dir/b.txt')), \
             (5, '/dir', MD5('/dir')); \
             INSERT INTO `{{ns}}_repository`.repository_pathchange (repositoryID, pathID, \
               commitID, targetPathID, targetCommitID, changeType, fileType, isDirect, \
               commitSequence) VALUES \
             (1, 1, 10, NULL, NULL, 2, 1, 1, 2), (1, 2, 10, NULL, NULL, 1, 1, 1, 2), \
             (1, 3, 10, NULL, NULL, 1, 5, 1, 2), (1, 5, 10, NULL, NULL, 10, 4, 0, 2), \
             (1, 4, 11, NULL, NULL, 1, 1, 1, 5); \
             INSERT INTO `{{ns}}_audit`.audit_transaction (phid, authorPHID, objectPHID, \
               viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, \
               newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-CMIT-1', 'PHID-APPS-x', 'PHID-CMIT-2', 'users', 'users', NULL, 0, \
               'audit:commit', 'null', '{value}', '{{}}', '{{}}', 1794600100, 1794600100); \
             INSERT INTO `{{ns}}_repository`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-CMIT-2', 32, 'PHID-DREV-1', 1, 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, repositoryPHID, activeDiffPHID) VALUES \
             (1, 'Add a.txt', 'PHID-DREV-1', 'published', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', '{{}}', 'PHID-REPO-git', 'PHID-DIFF-1'), \
             (2, 'Edit the README', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, \
               {NOW}, '{{}}', 'k', 'users', 'users', '{{}}', 'PHID-REPO-git', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_affectedpath (repositoryID, pathID, \
               revisionID) VALUES (1, 1, 2); \
             INSERT INTO `{{ns}}_repository`.repository_parents (childCommitID, parentCommitID) \
               VALUES (11, 12), (12, 10), (10, 13); \
             INSERT INTO `{{ns}}_repository`.repository_summary (repositoryID, size, lastCommitID, \
               epoch) VALUES (1, 4, 11, 1794600400); \
             INSERT INTO `{{ns}}_repository`.repository_statusmessage (repositoryID, statusType, \
               statusCode, parameters, epoch, messageCount) VALUES \
             (1, 'init', 'okay', '{{}}', {NOW}, 0), (1, 'fetch', 'okay', '{{}}', {NOW}, 0); \
             INSERT INTO `{{ns}}_daemon`.daemon_log (daemon, host, pid, argv, explicitArgv, \
               dateCreated, dateModified, status, daemonID) VALUES \
             ('PhabricatorRepositoryPullLocalDaemon', 'h', 1, '[]', '[]', 1, {NOW}, 'run', 'd1')",
            alice = session_key("alice-token"),
            root = session_key("root-token"),
            expires = NOW + 29 * 86_400,
            value = commit_value.replace('\'', "''"),
        ))
        .await;
    Some(Fixture {
        test_db,
        _directory: directory,
    })
}

async fn get_with(
    fixture: &Fixture,
    path: &str,
    cookie: &str,
    user_agent: Option<&str>,
) -> Response {
    let config = Config::default();
    let state = Arc::new(State {
        database: fixture.test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let mut request = Request::get(path).header(header::COOKIE, cookie);
    if let Some(agent) = user_agent {
        request = request.header(header::USER_AGENT, agent);
    }
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn get(fixture: &Fixture, path: &str, cookie: &str) -> Response {
    get_with(fixture, path, cookie, None).await
}

async fn post(fixture: &Fixture, path: &str, body: &str) -> Response {
    let config = Config::default();
    let state = Arc::new(State {
        database: fixture.test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let request = Request::post(path)
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

/// The payload of an `AphrontAjaxResponse`.
async fn payload(fixture: &Fixture, path: &str) -> serde_json::Value {
    let text = page(fixture, path, ALICE).await;
    let Some(json) = text.strip_prefix("for (;;);") else {
        fail(format!("{path}: no JSON shield: {text}"));
        return serde_json::Value::Null;
    };
    let envelope: serde_json::Value = serde_json::from_str(json).unwrap();
    envelope["payload"].clone()
}

async fn page(fixture: &Fixture, path: &str, cookie: &str) -> String {
    let response = get(fixture, path, cookie).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    if status != StatusCode::OK {
        fail(format!("{path} as {cookie}: {status} {reason:?} {text}"));
    }
    text
}

async fn fallback(fixture: &Fixture, path: &str) -> String {
    let response = get(fixture, path, ALICE).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn redirect(fixture: &Fixture, path: &str) -> String {
    let response = get(fixture, path, ALICE).await;
    if response.status() != StatusCode::FOUND {
        fail(format!(
            "{path}: {} instead of a redirect",
            response.status()
        ));
    }
    response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

async fn home_and_policies(fixture: &Fixture) {
    let page = page(fixture, "/diffusion/GIT/", ALICE).await;
    check!(
        page.contains("<title>Git · rGIT · Phabricator</title>"),
        page
    );
    check!(page.contains("Active"), page);
    check!(page.contains("The <strong>test</strong> repository"), page);
    // The top directory of master, and the README.
    check!(page.contains("<h2><a href=\"/diffusion/GIT/browse/master/\">Git (master)</a></h2>"));
    check!(
        page.contains("href=\"/diffusion/GIT/browse/master/dir/\""),
        page
    );
    check!(page.contains("data-path=\"dir/\""), page);
    check!(page.contains("data-path=\"README\""), page);
    check!(page.contains("data-lastmodified=\"/diffusion/GIT/lastmodified/master/;"));
    check!(page.contains("href=\"/diffusion/GIT/history/master/README\""));
    check!(page.contains("<h2>README</h2>"), page);
    check!(page.contains("Hello"), page);
    // The branches to switch to.
    check!(page.contains("Branch: master"), page);
    check!(
        page.contains("href=\"/diffusion/GIT/browse/feature/\""),
        page
    );
    check!(page.contains("href=\"/diffusion/GIT/clone/\""), page);
    check!(page.contains("aria-current=\"page\"><span class=\"fa fa-code\""));

    // An unknown branch.
    let page = self::page(fixture, "/diffusion/GIT/repository/nope/", ALICE).await;
    check!(page.contains("No Such Branch"), page);
    check!(
        page.contains("There is no branch named ") && page.contains("nope"),
        page
    );

    // The canonical URI is the callsign's, or the short name's.
    check_eq!(redirect(fixture, "/diffusion/1/").await, "/diffusion/GIT/");
    check_eq!(
        redirect(fixture, "/diffusion/7/browse/master/README").await,
        "/source/slugged/browse/master/README"
    );
    let page = self::page(fixture, "/source/slugged/", ALICE).await;
    check!(
        page.contains("href=\"/source/slugged/browse/master/dir/\""),
        page
    );

    // A repository the viewer can't see, as PHP's policy exception.
    let response = get(fixture, "/diffusion/SEC/", ALICE).await;
    check_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = self::page(fixture, "/diffusion/SEC/", ROOT).await;
    check!(page.contains("Secret"), page);
    check_eq!(
        get(fixture, "/diffusion/NOPE/", ALICE).await.status(),
        StatusCode::NOT_FOUND
    );
    check_eq!(
        get(fixture, "/source/nope/", ALICE).await.status(),
        StatusCode::NOT_FOUND
    );

    // What PHP serves.
    check_eq!(
        fallback(fixture, "/diffusion/HG/").await,
        "Mercurial and Subversion repositories"
    );
    let response = get_with(fixture, "/diffusion/GIT/info/refs", ALICE, Some("git/2.43")).await;
    check_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    fallback(fixture, "/diffusion/GIT/manage/uris/").await;
    fallback(fixture, "/diffusion/GIT/browse/master/?grep=Hello").await;
    fallback(fixture, "/diffusion/GIT/browse/master/README?view=raw").await;
}

async fn browse(fixture: &Fixture) {
    let page = page(fixture, "/diffusion/GIT/browse/master/dir/", ALICE).await;
    check!(
        page.contains("<title>dir · rGIT · Phabricator</title>"),
        page
    );
    check!(page.contains("b.txt"), page);
    check!(!page.contains("a.txt"), page);
    check!(
        page.contains("href=\"/diffusion/GIT/history/master/dir/\""),
        page
    );
    check!(
        page.contains("<a href=\"/diffusion/GIT/browse/master/\">rGIT</a>"),
        page
    );
    check!(
        page.contains(&format!("href=\"/rGIT{C5}\">{}</a>", &C5[..12])),
        page
    );

    // A file, with its lines and the open revisions touching it.
    let page = self::page(fixture, "/diffusion/GIT/browse/master/README$2", ALICE).await;
    check!(
        page.contains("<title>README · rGIT · Phabricator</title>"),
        page
    );
    check!(
        page.contains(&format!(
            "<tr class=\"diffusion-selected\"><th class=\"phabricator-source-blame-skip\" \
             data-blame=\"skip\"></th><th class=\"phabricator-source-blame-info\" \
             data-blame=\"info\" data-blame-lines=\"2\"></th><th class=\"n\"><a \
             href=\"/diffusion/GIT/browse/master/README;{C5}$2\">2</a></th><td \
             class=\"code\">World</td></tr>"
        )),
        page
    );
    check!(page.contains("Recent Open Revisions"), page);
    check!(page.contains("<a href=\"/D2\">Edit the README</a>"), page);
    check!(
        page.contains("href=\"/diffusion/GIT/change/master/README\""),
        page
    );

    // A tag, and a path deleted at the commit.
    let page = self::page(fixture, "/diffusion/GIT/browse/master/;v2", ALICE).await;
    check!(page.contains("<dt>Tag</dt><dd>v2</dd>"), page);
    check!(page.contains("Version 2"), page);
    check!(page.contains("Back to HEAD"), page);
    // Git says the path doesn't exist at the commit, which PHP rethrows.
    fallback(fixture, "/diffusion/GIT/browse/master/dir/a.txt").await;
}

async fn history_and_refs(fixture: &Fixture) {
    let page = page(fixture, "/diffusion/GIT/history/master/", ALICE).await;
    check!(
        page.contains("<title>History · rGIT · Phabricator</title>"),
        page
    );
    for commit in [C5, C4, C3, C2, C1] {
        check!(
            page.contains(&format!("href=\"/rGIT{commit}\"")),
            "{commit}: {page}"
        );
    }
    check!(page.contains("Add a.txt and a link"), page);
    // C4 is not imported.
    check!(page.contains("<em>Discovering…</em>"), page);
    check!(page.contains("Revisions: <a href=\"/D1\""), page);
    check!(
        page.contains("Author: <a href=\"/p/alice/\">alice</a>"),
        page
    );
    check!(page.contains("Audit Required"), page);
    check!(page.contains("2026-11-13"), page);

    let page = self::page(fixture, "/diffusion/GIT/history/master/README", ALICE).await;
    check!(page.contains(&format!("href=\"/rGIT{C2}\"")), page);
    check!(!page.contains(&format!("href=\"/rGIT{C4}\"")), page);

    let page = self::page(fixture, "/diffusion/GIT/branches/", ALICE).await;
    let master = page.find(">master</a>").unwrap_or(usize::MAX);
    let feature = page.find(">feature</a>").unwrap_or(0);
    check!(master < feature, page);
    check!(page.contains("title=\"Default Branch\""), page);
    check!(page.contains("Merge branch "), page);
    check!(
        page.contains("href=\"/diffusion/GIT/compare/?head=feature\""),
        page
    );

    let page = self::page(fixture, "/diffusion/GIT/tags/", ALICE).await;
    check!(
        page.contains("href=\"/diffusion/GIT/history/master/;v1\">v1</a>"),
        page
    );
    check!(page.contains("Version 2"), page);
    check!(
        page.contains("href=\"/diffusion/GIT/browse/master/;v2\""),
        page
    );
}

async fn commits(fixture: &Fixture) {
    let page = page(fixture, &format!("/rGIT{C2}"), ALICE).await;
    check!(page.contains("Add a.txt and a link</h1>"), page);
    check!(page.contains("See <a"), page);
    check!(
        page.contains(
            "<a href=\"/p/alice/\">alice</a> <span class=\"muted\">Authored on 2026-11-13"
        )
    );
    check!(
        page.contains("<dt>Differential Revision</dt><dd><a href=\"/D1\""),
        page
    );
    check!(
        page.contains(&format!("<dt>Parents</dt><dd><a href=\"/rGIT{C1}\"")),
        page
    );
    check!(page.contains("tag: v2"), page);
    check!(
        page.contains("data-php-fragment=\"/diffusion/GIT/commit/"),
        page
    );
    check!(page.contains("committed <a href"), page);
    check!(page.contains("Changes (3)"), page);
    check!(page.contains("data-path=\"dir/a.txt\""), page);
    check!(page.contains("World"), page);
    // A short hash names the same commit.
    let short = self::page(fixture, &format!("/rGIT{}", &C2[..12]), ALICE).await;
    check!(short.contains("Add a.txt and a link</h1>"), short);

    let page = self::page(fixture, &format!("/rGIT{C5}"), ALICE).await;
    check!(page.contains("Merged Changes"), page);
    check!(page.contains(&format!("href=\"/rGIT{C3}\"")), page);
    check!(page.contains("data-path=\"dir/b.txt\""), page);

    check_eq!(
        redirect(fixture, &format!("/R1:{C2}")).await,
        format!("/rGIT{C2}")
    );
    check_eq!(
        redirect(fixture, &format!("/source/slugged/commit/{C2}")).await,
        format!("/R7:{C2}")
    );
    check_eq!(
        fallback(fixture, &format!("/rGIT{C3}")).await,
        "an unpublished commit"
    );
    check_eq!(
        fallback(fixture, &format!("/rGIT{C1}")).await,
        "a commit with audits"
    );
    fallback(fixture, &format!("/rGIT{C4}")).await;
    fallback(fixture, &format!("/rGIT{C2}?diff=1")).await;
    check_eq!(
        get(fixture, &format!("/rSEC{C2}"), ALICE).await.status(),
        StatusCode::FORBIDDEN
    );
}

async fn scripts(fixture: &Fixture) {
    // The last change of directory entries; the graph cache answers for
    // dir/a.txt from the imported changes, where Git would name C4.
    let modified = payload(
        fixture,
        "/diffusion/GIT/lastmodified/master/?paths=%5B%22README%22%2C%22dir%2Fa.txt%22%5D&__ajax__=1",
    )
    .await;
    let commit = format!("<a href=\"/rGIT{C2}\">rGIT{}</a>", &C2[..12]);
    check_eq!(modified["README"]["commit"], json!(commit));
    check_eq!(modified["dir/a.txt"]["commit"], json!(commit));
    check!(
        modified["README"]["details"]
            .as_str()
            .unwrap_or_default()
            .contains("Add a.txt and a link</a></span>\u{a0}</div>")
    );
    let response = post(
        fixture,
        "/diffusion/GIT/lastmodified/master/",
        "__csrf__=nope&paths=%5B%22README%22%5D",
    )
    .await;
    check_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);

    // The branches and tags of a commit.
    let branches = payload(fixture, &format!("/diffusion/GIT/commit/{C2}/branches/")).await;
    let branches = branches.as_str().unwrap_or_default();
    check!(
        branches.contains(&format!(
            "href=\"/diffusion/GIT/browse/feature/;{C2}\">feature</a>"
        )),
        branches
    );
    check!(branches.contains(">master</a>"), branches);
    let tags = payload(
        fixture,
        &format!("/diffusion/GIT/commit/{C2}/tags/?__ajax__=1"),
    )
    .await;
    check_eq!(
        tags,
        json!("<a href=\"/diffusion/GIT/browse/master/;v2\">v2</a>")
    );
    check_eq!(
        payload(fixture, &format!("/diffusion/GIT/commit/{C5}/tags/")).await,
        json!("None")
    );

    // Blame, with the revision of C2.
    let blame = payload(fixture, "/diffusion/GIT/blame/master/README").await;
    check_eq!(blame["blame"], json!([C1, C2]));
    let info = blame["map"][C2]["info"].as_str().unwrap_or_default();
    check!(info.contains(&format!("href=\"/rGIT{C2}\"")), info);
    check!(info.contains("<a href=\"/D1\""), info);
    check!(info.contains("href=\"/p/alice/\""), info);
    check!(
        blame["map"][C1]["skip"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!("?before={C1}")),
        blame
    );
    check_eq!(
        blame["epoch"],
        json!({"min": 1_794_600_000, "max": 1_794_600_100})
    );

    // The paths of "Locate File".
    check_eq!(
        payload(fixture, "/diffusion/GIT/pathtree/master/").await,
        json!({"tree": {"README": 1, "dir/": {"b.txt": 1}, "link": 1}})
    );
    let home = page(fixture, "/diffusion/GIT/", ALICE).await;
    check!(
        home.contains("data-pathtree=\"/diffusion/GIT/pathtree/master/\""),
        home
    );

    // The file page loads its blame, unless it's off.
    let file = page(fixture, "/diffusion/GIT/browse/master/README", ALICE).await;
    check!(
        file.contains("data-blame-uri=\"/diffusion/GIT/blame/master/README;"),
        file
    );
    check!(file.contains("data-blame-lines=\"2\""), file);
    check!(file.contains("Hide Blame"), file);
    let file = page(
        fixture,
        "/diffusion/GIT/browse/master/README?blame=off",
        ALICE,
    )
    .await;
    check!(!file.contains("data-blame-uri"), file);
    check!(file.contains("Show Blame"), file);

    // The graph of the history of a branch, but not of a path.
    let history = page(fixture, "/diffusion/GIT/history/master/", ALICE).await;
    check!(history.contains("data-graph-count=\"2\""), history);
    check!(
        history.contains("data-graph=\"{&#34;line&#34;:&#34;^&#34;,&#34;split&#34;:[1]"),
        history
    );
    let history = page(fixture, "/diffusion/GIT/history/master/README", ALICE).await;
    check!(!history.contains("data-graph=\""), history);
}

async fn change_and_compare(fixture: &Fixture) {
    let page = page(fixture, "/diffusion/GIT/change/master/README", ALICE).await;
    check!(
        page.contains("<title>README · rGIT · Phabricator</title>"),
        page
    );
    check!(
        page.contains(&format!("href=\"/rGIT{C2}\">{}</a>", &C2[..12])),
        page
    );
    check!(page.contains("World"), page);
    check!(
        page.contains(&format!(
            "href=\"/diffusion/GIT/history/master/README;{C2}\""
        )),
        page
    );

    let page = self::page(
        fixture,
        "/diffusion/GIT/compare/?head=master&against=feature",
        ALICE,
    )
    .await;
    check!(
        page.contains("Changes on <em>master</em> but not <em>feature</em>"),
        page
    );
    check!(page.contains(&format!("href=\"/rGIT{C5}\"")), page);
    check!(page.contains(&format!("href=\"/rGIT{C4}\"")), page);
    check!(!page.contains(&format!("href=\"/rGIT{C3}\"")), page);
    check!(page.contains("data-graph="), page);
    check!(
        page.contains("href=\"/diffusion/GIT/compare/?head=feature&#38;against=master\""),
        page
    );
    let page = self::page(
        fixture,
        "/diffusion/GIT/compare/?head=feature&against=master",
        ALICE,
    )
    .await;
    check!(page.contains("Up To Date"), page);
    // A default the viewer must confirm, as PHP's dialog.
    let page = self::page(fixture, "/diffusion/GIT/compare/?head=feature", ALICE).await;
    check!(page.contains("Compare Against"), page);
    check!(page.contains("name=\"against\" value=\"master\""), page);
    let page = self::page(fixture, "/diffusion/GIT/browse/master/dir/", ALICE).await;
    check!(
        page.contains("href=\"/diffusion/GIT/compare/?head=master\""),
        page
    );
}

async fn list_and_manage(fixture: &Fixture) {
    let page = page(fixture, "/diffusion/", ALICE).await;
    check!(page.contains("<a href=\"/diffusion/GIT/\">Git</a>"), page);
    check!(page.contains("4 Commits"), page);
    check!(
        page.contains(&format!("{}: Merge branch &#39;feature&#39;", &C5[..12]))
            || page.contains(&format!("{}: Merge branch &#x27;feature&#x27;", &C5[..12])),
        page
    );
    // Inactive repositories are in "All", and Secret only for admins.
    check!(!page.contains("Slugged"), page);
    let page = self::page(fixture, "/diffusion/query/all/", ALICE).await;
    check!(
        page.contains("<a href=\"/source/slugged/\">Slugged</a>"),
        page
    );
    check!(page.contains("No Commits"), page);
    check!(!page.contains("Secret"), page);
    let page = self::page(fixture, "/diffusion/query/all/", ROOT).await;
    check!(page.contains("Secret"), page);
    fallback(fixture, "/diffusion/query/abcdef/").await;
    fallback(fixture, "/diffusion/?status=open").await;

    let page = self::page(fixture, "/diffusion/GIT/manage/", ALICE).await;
    check!(
        page.contains("<title>Basics · rGIT · Phabricator</title>"),
        page
    );
    check!(page.contains("Repository Active"), page);
    check!(page.contains("<dt>Callsign</dt><dd>GIT</dd>"), page);
    check!(page.contains("Found Binary <tt>git</tt>"), page);
    check!(page.contains("Pull Daemon Running"), page);
    check!(page.contains("Task Daemon Not Running"), page);
    check!(page.contains("Working Copy OK"), page);
    check!(page.contains("Updates OK"), page);
    check!(page.contains("Fully Imported"), page);
    check!(page.contains("The <strong>test</strong> repository"), page);
    // Alice can't edit the repository.
    check!(
        page.contains("<span class=\"muted\"><span class=\"fa fa-pencil\""),
        page
    );
    let page = self::page(fixture, "/diffusion/GIT/manage/policies/", ALICE).await;
    check!(page.contains("All Users</a>"), page);
    check!(page.contains("Administrators</a>"), page);
    check!(page.contains("Not a Hosted Repository"), page);
    let page = self::page(fixture, "/diffusion/GIT/manage/branches/", ALICE).await;
    check!(
        page.contains("<dt>Default Branch</dt><dd><em>master</em></dd>"),
        page
    );
    check!(page.contains("<em>All Branches</em>"), page);
    let page = self::page(fixture, "/diffusion/GIT/manage/limits/", ALICE).await;
    check!(page.contains("<em>Default (900 Seconds)</em>"), page);
    fallback(fixture, "/diffusion/GIT/manage/uris/").await;
    fallback(fixture, "/diffusion/GIT/manage/history/").await;
    check_eq!(
        get(fixture, "/diffusion/GIT/manage/nope/", ALICE)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn diffusion_pages() {
    let Some(fixture) = setup().await else {
        return;
    };
    home_and_policies(&fixture).await;
    browse(&fixture).await;
    history_and_refs(&fixture).await;
    commits(&fixture).await;
    scripts(&fixture).await;
    change_and_compare(&fixture).await;
    list_and_manage(&fixture).await;
    let Fixture {
        test_db,
        _directory: directory,
    } = fixture;
    test_db.drop_databases().await;
    drop(directory);
    let failures = FAILURES.lock().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
