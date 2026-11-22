//! The Git query methods (`diffusion.branchquery`, `diffusion.historyquery`,
//! ...) against a test database and the working copy
//! `scripts/seed-dev-git-repo.sh` builds.

use std::path::Path;

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-gqgqgqgqgqgqgqgqgqgqgqgqgqgq";

const C1: &str = "d61f24fe3e14bb3307532a9bc02968d7462cb336";
const C2: &str = "85948bd7ad4beca2df6fabad0d7f08b90365eff5";
const C3: &str = "59965686ade70d003a571d2352862b01b4c3c3b8";
const C4: &str = "20fb56cd8ff000f2f791dc647906b4c5a9bef267";
const C5: &str = "81bcc2f65c2b5d5a68333fc3df1f7dfe77482aa4";
/// The annotated tag object of `v2`.
const TAG_V2: &str = "4b0dc5859ad5c57133cd407645d12a2d09589b1d";

const DATABASES: &[&str] = &[
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "cache",
];

struct Fixture {
    test_db: TestDatabase,
    context: Context,
    _directory: tempfile::TempDir,
}

async fn call_method(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_diffusion::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(
        context,
        &registry,
        method,
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

/// The result of a call to the Git repository, which must succeed.
async fn answer(context: &Context, method: &str, params: Value) -> Value {
    let mut params = params;
    let takes_repository = !matches!(
        method,
        "diffusion.getrecentcommitsbypath" | "diffusion.getlintmessages"
    );
    if takes_repository && params.get("repository").is_none() {
        params["repository"] = json!("GIT");
    }
    match call_method(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

/// The error of a call to the Git repository.
async fn error(context: &Context, method: &str, params: Value) -> (String, Value) {
    match call_method(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info,
            ..
        }) => (code, json!(error_info)),
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

async fn is_fallback(context: &Context, method: &str, params: Value) -> bool {
    matches!(
        call_method(context, method, params).await,
        Response::Fallback(_)
    )
}

fn seed_repository(directory: &Path) {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/seed-dev-git-repo.sh");
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
    let test_db = TestDatabase::create(DATABASES).await?;
    let directory = tempfile::tempdir().expect("a temporary directory");
    let copy = directory.path().join("2");
    seed_repository(&copy);
    // localPath is unique: each repository spells the path its own way.
    let base = copy.display().to_string();
    let copy = format!("{base}/");
    let (hg, secret, cluster, filtered) = (
        format!("{base}/."),
        format!("{base}/./"),
        format!("{base}/./."),
        format!("{base}/././"),
    );
    let feature_hash = phab_core::hash::digest_for_index(b"feature");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy, localPath, almanacServicePHID) VALUES \
             (1, 'PHID-REPO-git', 'Git', 'GIT', 'git', '{{}}', 1, 1, 'users', 'admin', 'admin', \
               '{copy}', NULL), \
             (2, 'PHID-REPO-hg', 'Hg', 'HG', 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin', \
               '{hg}', NULL), \
             (3, 'PHID-REPO-secret', 'Secret', 'SEC', 'git', '{{}}', 1, 1, 'admin', 'admin', \
               'admin', '{secret}', NULL), \
             (4, 'PHID-REPO-gone', 'Gone', 'GONE', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin', '/nonexistent/repo/', NULL), \
             (5, 'PHID-REPO-cluster', 'Cluster', 'CLU', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin', '{cluster}', 'PHID-ALMS-x'), \
             (6, 'PHID-REPO-filtered', 'Filtered', 'FIL', 'git', \
               '{{\"branch-filter\":{{\"feature\":true}}}}', 1, 1, 'users', 'admin', 'admin', \
               '{filtered}', NULL); \
             INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, summary, importStatus, authorPHID) VALUES \
             (10, 1, 'PHID-CMIT-2', '{C2}', 1794600100, 'none', 'Add a.txt', 15, NULL); \
             INSERT INTO `{{ns}}_repository`.repository_commitdata (commitID, authorName, \
               commitMessage, commitDetails) VALUES \
             (10, 'alice', 'Add a.txt and a link', '{{\"ref\":{{\"authorName\":\"a/b\"}}}}'); \
             INSERT INTO `{{ns}}_repository`.repository_refcursor (id, phid, repositoryPHID, \
               refType, refNameHash, refNameRaw, refNameEncoding, isPermanent) VALUES \
             (1, 'PHID-RREF-1', 'PHID-REPO-git', 'branch', '{feature_hash}', 'feature', 'utf8', 0); \
             INSERT INTO `{{ns}}_repository`.repository_refposition (cursorID, commitIdentifier, \
               isClosed) VALUES (1, '{C3}', 0); \
             INSERT INTO `{{ns}}_repository`.repository_branch (id, repositoryID, name, \
               lintCommit, dateCreated, dateModified) VALUES (1, 1, 'master', '{C5}', 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository_lintmessage (branchID, path, line, code, \
               severity, name, description) VALUES \
             (1, 'README', 2, 'X1', 'error', 'Lint', 'Bad'), \
             (1, 'other', 1, 'X2', 'error', 'Lint', 'Bad')"
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some(Fixture {
        test_db,
        context,
        _directory: directory,
    })
}

fn names(result: &Value, key: &str) -> Vec<String> {
    result
        .as_array()
        .expect("a list")
        .iter()
        .map(|item| item[key].as_str().expect("a name").to_owned())
        .collect()
}

async fn lists_branches_and_tags(context: &Context) {
    let branches = answer(context, "diffusion.branchquery", json!({})).await;
    assert_eq!(names(&branches, "shortName"), ["master", "feature"]);
    assert_eq!(
        branches[0],
        json!({
            "shortName": "master",
            "commitIdentifier": C5,
            "refType": "branch",
            "rawFields": {
                "objectname": C5,
                "objecttype": "commit",
                "refname": "refs/heads/master",
                "*objectname": "",
                "*objecttype": "",
                "subject": "Merge branch 'feature'",
                "creator": "alice <alice@example.com> 1794600400 +0000",
                "author": "alice <alice@example.com>",
                "epoch": 1_794_600_400,
            },
        })
    );
    for (params, expected) in [
        (json!({"limit": 1}), vec!["master"]),
        (json!({"offset": "1"}), vec!["feature"]),
        (json!({"closed": true}), vec![]),
        (json!({"closed": false}), vec!["master", "feature"]),
        (json!({"repository": "FIL"}), vec!["feature"]),
    ] {
        let found = answer(context, "diffusion.branchquery", params.clone()).await;
        assert_eq!(names(&found, "shortName"), expected, "{params}");
    }
    assert_eq!(
        answer(context, "diffusion.branchquery", json!({"contains": C3})).await,
        json!([
            {"shortName": "feature", "commitIdentifier": C3, "refType": null, "rawFields": []},
            {"shortName": "master", "commitIdentifier": C5, "refType": null, "rawFields": []},
        ])
    );

    let tags = answer(
        context,
        "diffusion.tagsquery",
        json!({"needMessages": true}),
    )
    .await;
    assert_eq!(
        tags,
        json!([
            {
                "author": "alice <alice@example.com>",
                "epoch": 1_794_600_100,
                "commitIdentifier": C2,
                "name": "v2",
                "description": "Version 2",
                "type": "git/tag",
                "message": "Version 2\n",
            },
            {
                "author": "alice <alice@example.com>",
                "epoch": 1_794_600_000,
                "commitIdentifier": C1,
                "name": "v1",
                "description": "Add the README",
                "type": "git/commit",
                "message": null,
            },
        ])
    );
    for (params, expected) in [
        (json!({"names": ["v1", "v3"]}), vec!["v1"]),
        (json!({"commit": C2}), vec!["v2"]),
        (json!({"commit": "nope"}), vec![]),
        (json!({"limit": 1, "offset": 1}), vec!["v1"]),
    ] {
        let found = answer(context, "diffusion.tagsquery", params.clone()).await;
        assert_eq!(names(&found, "name"), expected, "{params}");
        assert!(found[0].get("message").is_none());
    }

    assert_eq!(
        answer(context, "diffusion.refsquery", json!({"commit": C2})).await,
        json!([{"ref": "tag: v2", "href": {}}])
    );
    assert_eq!(
        answer(context, "diffusion.refsquery", json!({"commit": C4})).await,
        json!([])
    );
}

async fn resolves_refs_like_php(context: &Context) {
    let resolve = |refs: Value, types: Value| {
        answer(
            context,
            "diffusion.resolverefs",
            json!({"refs": refs, "types": types}),
        )
    };

    assert_eq!(
        resolve(json!(["master", "v2"]), json!(null)).await,
        json!({
            "master": [{"type": "branch", "identifier": C5}],
            "v2": [{"type": "tag", "identifier": C2, "alternate": TAG_V2}],
        })
    );
    assert_eq!(resolve(json!(["master"]), json!(["tag"])).await, json!([]));
    // A ref Git must resolve discards the names resolved before it, and
    // every commit takes the identifier of the last line.
    assert_eq!(
        resolve(json!(["master", "HEAD~1", "HEAD~2", "nope"]), json!([])).await,
        json!({
            "HEAD~1": [{"type": "commit", "identifier": "nope"}],
            "HEAD~2": [{"type": "commit", "identifier": "nope"}],
        })
    );
    assert_eq!(
        resolve(json!(["HEAD~1", "HEAD~2"]), json!(null)).await,
        json!({
            "HEAD~1": [{"type": "commit", "identifier": C2}],
            "HEAD~2": [{"type": "commit", "identifier": C2}],
        })
    );
    assert_eq!(
        resolve(json!([TAG_V2]), json!(null)).await,
        json!({TAG_V2: [{"type": "tag", "identifier": TAG_V2}]})
    );
    assert_eq!(resolve(json!([]), json!(null)).await, json!([]));

    assert_eq!(
        answer(context, "diffusion.existsquery", json!({"commit": C1})).await,
        json!(true)
    );
    assert_eq!(
        answer(context, "diffusion.existsquery", json!({"commit": "nope"})).await,
        json!(false)
    );
    assert_eq!(
        answer(
            context,
            "diffusion.commitparentsquery",
            json!({"commit": C5})
        )
        .await,
        json!([C4, C3])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.commitparentsquery",
            json!({"commit": C1})
        )
        .await,
        json!([""])
    );
}

/// A path change of a commit which is not imported.
fn ephemeral(hash: &str) -> Value {
    json!({
        "path": null,
        "commitIdentifier": hash,
        "commit": {
            "repositoryID": "1",
            "phid": null,
            "commitIdentifier": hash,
            "epoch": null,
            "authorPHID": null,
            "auditStatus": "none",
            "summary": "",
            "importStatus": 0,
        },
        "commitData": [],
        "fileType": null,
        "changeType": null,
        "targetPath": null,
        "targetCommitIdentifier": null,
        "awayPaths": [],
    })
}

async fn reads_history(context: &Context) {
    let history = answer(
        context,
        "diffusion.historyquery",
        json!({"commit": "master", "path": "dir/a.txt", "limit": 10}),
    )
    .await;
    assert_eq!(history["pathChanges"][0], ephemeral(C4));
    assert_eq!(
        history["pathChanges"][1],
        json!({
            "path": null,
            "commitIdentifier": C2,
            "commit": {
                "repositoryID": "1",
                "phid": "PHID-CMIT-2",
                "commitIdentifier": C2,
                "epoch": "1794600100",
                "authorPHID": null,
                "auditStatus": "none",
                "summary": "Add a.txt",
                "importStatus": "15",
            },
            "commitData": {
                "commitID": "10",
                "authorName": "alice",
                "commitMessage": "Add a.txt and a link",
                "commitDetails": r#"{"ref":{"authorName":"a\/b"}}"#,
            },
            "fileType": null,
            "changeType": null,
            "targetPath": null,
            "targetCommitIdentifier": null,
            "awayPaths": [],
        })
    );
    assert_eq!(history["parents"], json!({C4: [C2], C2: [C1]}));
    let all = answer(
        context,
        "diffusion.historyquery",
        json!({"commit": C5, "path": "", "limit": 2, "offset": 1}),
    )
    .await;
    assert_eq!(all["parents"], json!({C4: [C2], C3: [C2]}));
    let none = answer(
        context,
        "diffusion.historyquery",
        json!({"commit": C1, "against": C1, "path": "", "limit": 2}),
    )
    .await;
    assert_eq!(none, json!({"pathChanges": [], "parents": []}));
}

async fn finds_commits_of_paths(context: &Context) {
    assert_eq!(
        answer(
            context,
            "diffusion.mergedcommitsquery",
            json!({"commit": C5})
        )
        .await,
        json!([ephemeral(C3)])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.mergedcommitsquery",
            json!({"commit": C4})
        )
        .await,
        json!([])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.lastmodifiedquery",
            json!({"paths": {"README": "master", "dir": C5}}),
        )
        .await,
        json!({"README": C2, "dir": C5})
    );
    assert_eq!(
        answer(
            context,
            "diffusion.blame",
            json!({"paths": ["README", "missing"], "commit": "master"}),
        )
        .await,
        json!({"README": [C1, C2]})
    );
    assert_eq!(
        answer(
            context,
            "diffusion.getrecentcommitsbypath",
            json!({"callsign": "GIT", "path": "README"}),
        )
        .await,
        json!([format!("rGIT{C2}"), format!("rGIT{C1}")])
    );
    // The cached ref cursor of "feature".
    assert_eq!(
        answer(
            context,
            "diffusion.getrecentcommitsbypath",
            json!({"callsign": "GIT", "path": "dir", "branch": "feature", "limit": 1}),
        )
        .await,
        json!([format!("rGIT{C3}")])
    );
    assert_eq!(
        error(
            context,
            "diffusion.getrecentcommitsbypath",
            json!({"callsign": "NOPE", "path": ""}),
        )
        .await,
        (
            "ERR_NOT_FOUND".to_owned(),
            json!("Repository was not found.")
        )
    );
}

async fn browses(context: &Context) {
    let root = answer(
        context,
        "diffusion.browsequery",
        json!({"commit": "master"}),
    )
    .await;
    assert_eq!(names(&root["paths"], "path"), ["README", "dir", "link"]);
    assert_eq!(
        (&root["paths"][1]["fileType"], &root["paths"][1]["fileSize"]),
        (&json!(4), &json!("-"))
    );
    assert_eq!(root["paths"][2]["fileType"], json!(5));
    assert_eq!(root["isValidResults"], json!(true));
    let directory = answer(
        context,
        "diffusion.browsequery",
        json!({"commit": C3, "path": "dir", "offset": 1}),
    )
    .await;
    assert_eq!(
        directory["paths"],
        json!([{
            "fullPath": "dir/b.txt",
            "path": "b.txt",
            "hash": "af9c6fd168ea28cf99aa2c2dd9057a8b720e2262",
            "fileType": 7,
            "fileSize": "4",
            "externalURI": null,
            "lastModifiedCommit": null,
            "lastCommitData": null,
        }])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.browsequery",
            json!({"commit": "master", "path": "README"}),
        )
        .await,
        json!({
            "paths": [],
            "isValidResults": false,
            "reasonForEmptyResultSet": "is-file",
            "existedAtCommit": null,
            "deletedAtCommit": null,
        })
    );
}

async fn searches(context: &Context) {
    assert_eq!(
        answer(
            context,
            "diffusion.searchquery",
            json!({"path": "", "commit": "master", "grep": "orl|bee", "limit": 10}),
        )
        .await,
        json!([["README", "2", "World"], ["dir/b.txt", "1", "bee"]])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.searchquery",
            json!({"path": "", "grep": "nothing here", "limit": 10}),
        )
        .await,
        json!([])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.querypaths",
            json!({"path": "", "commit": "master", "limit": 2, "offset": 1}),
        )
        .await,
        json!(["dir/b.txt", "link"])
    );
    assert_eq!(
        answer(
            context,
            "diffusion.querypaths",
            json!({"path": "", "commit": "master", "pattern": r"\.txt$"}),
        )
        .await,
        json!(["dir/b.txt"])
    );
    let raw = answer(
        context,
        "diffusion.internal.gitrawdiffquery",
        json!({"commit": C2}),
    )
    .await;
    assert!(
        raw.as_str()
            .is_some_and(|raw| raw.contains("\tdir/a.txt\n")),
        "{raw}"
    );
    let first = answer(
        context,
        "diffusion.internal.gitrawdiffquery",
        json!({"commit": C1}),
    )
    .await;
    assert!(
        first.as_str().is_some_and(|raw| raw.contains("\tREADME")),
        "{first}"
    );

    assert_eq!(
        answer(
            context,
            "diffusion.getlintmessages",
            json!({"repositoryPHID": "PHID-REPO-git", "branch": "", "files": ["README"]}),
        )
        .await,
        json!([{"path": "README", "line": "2", "code": "X1"}])
    );
}

async fn leaves_the_rest_to_php(context: &Context) {
    for repository in ["HG", "SEC", "GONE", "CLU"] {
        assert!(
            is_fallback(
                context,
                "diffusion.branchquery",
                json!({"repository": repository}),
            )
            .await,
            "{repository}"
        );
    }
    for (method, params) in [
        // Git doesn't say "Not a valid object name" any more.
        (
            "diffusion.browsequery",
            json!({"repository": "GIT", "commit": "master", "path": "nope"}),
        ),
        // The command fails, which PHP reports with its command line.
        (
            "diffusion.historyquery",
            json!({"repository": "GIT", "commit": "nope", "path": "", "limit": 1}),
        ),
        (
            "diffusion.blame",
            json!({"repository": "GIT", "paths": ["README"], "commit": "master", "timeout": 5}),
        ),
        (
            "diffusion.querypaths",
            json!({"repository": "GIT", "path": "", "commit": "master", "pattern": "a)b("}),
        ),
    ] {
        assert!(
            is_fallback(context, method, params.clone()).await,
            "{params}"
        );
    }

    let core = |message: &str| ("ERR-CONDUIT-CORE".to_owned(), json!(message));
    assert_eq!(
        error(
            context,
            "diffusion.branchquery",
            json!({"repository": "NOPE"})
        )
        .await,
        core("Repository \"NOPE\" is not a valid repository.")
    );
    assert_eq!(
        error(context, "diffusion.branchquery", json!({})).await,
        core("One of \"repository\" and \"callsign\" is required.")
    );
    assert_eq!(
        error(
            context,
            "diffusion.refsquery",
            json!({"repository": "GIT", "commit": "--all"}),
        )
        .await,
        core(
            "Git ref selector \"--all\" is not a valid selector and can not be passed to the Git \
             CLI safely in the general case."
        )
    );
    assert_eq!(
        error(
            context,
            "diffusion.searchquery",
            json!({"repository": "GIT", "path": "", "commit": "nope", "grep": "x"}),
        )
        .await,
        core("Ref \"nope\" does not exist in this repository.")
    );
}

/// One test, so the databases are built once.
#[tokio::test]
async fn answers_from_the_working_copy() {
    let Some(fixture) = setup().await else {
        return;
    };
    let context = &fixture.context;
    lists_branches_and_tags(context).await;
    resolves_refs_like_php(context).await;
    reads_history(context).await;
    finds_commits_of_paths(context).await;
    browses(context).await;
    searches(context).await;
    leaves_the_rest_to_php(context).await;
    fixture.test_db.drop_databases().await;
}
