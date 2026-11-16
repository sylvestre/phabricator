//! Remarkup with object references resolved against a test database.

use phab_core::{Config, Phid};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_policy::Viewer;
use phab_remarkup::Mode;
use phab_web::WebState;

struct State {
    database: Database,
    config: Config,
    remarkup: phab_remarkup::Engine,
    php: Option<phab_web::php::PhpConduit>,
}

impl WebState for State {
    fn database(&self) -> &Database {
        &self.database
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn now(&self) -> i64 {
        1_700_000_000
    }

    fn remarkup(&self) -> &phab_remarkup::Engine {
        &self.remarkup
    }

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        self.php.as_ref()
    }
}

#[tokio::test]
async fn references_resolve_under_policy() {
    let Some(test_db) = TestDatabase::create(&[
        "user",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
    ])
    .await
    else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'Bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open <one>', 'PHID-DREV-one', 'published', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', '{}', ''), \
             (2, 'Secret', 'PHID-DREV-two', 'needs-review', '', '', 'PHID-USER-bob', 1, 1, '{}', 'k', \
               'no-one', 'no-one', '{}', ''); \
             INSERT INTO `{ns}_project`.project (name, phid, authorPHID, dateCreated, dateModified, status, \
               viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, hasMilestones, \
               hasSubprojects, projectPath, projectDepth, projectPathKey, properties, subtype, primarySlug) VALUES \
             ('Firefox Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', 'blue', \
               'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', 'firefox-build'), \
             ('Secret team', 'PHID-PROJ-team', 'x', 1, 1, '0', 'no-one', 'users', 'users', 'fa-x', 'blue', \
               'k', 0, 0, 0, 'b', 0, 'k002', '{}', 'default', 'team'); \
             INSERT INTO `{ns}_project`.project_slug (projectPHID, slug, dateCreated, dateModified) VALUES \
               ('PHID-PROJ-build', 'firefox-build', 1, 1), ('PHID-PROJ-team', 'team', 1, 1)",
        )
        .await;
    let config = Config::default();
    let state = State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        php: None,
        config,
    };
    let text = "D1, D2, D3 and @bob, @carol. bug 42 #Firefox-Build #team";

    let as_alice = Viewer::user(Phid::new("PHID-USER-alice"), false);
    let html = phab_web::remarkup::render(&state, &as_alice, text, Mode::Html)
        .await
        .unwrap();
    let html = String::from_utf8(html.into_bytes()).unwrap();
    assert!(html.contains(r#"<a href="/D1" class="remarkup-object remarkup-object-closed" title="D1: Open &lt;one&gt;""#), "{html}");
    // Alice can't see D2: it stays text, like D3 which doesn't exist.
    assert!(html.contains(", D2, D3 and "), "{html}");
    assert!(!html.contains("Secret"), "{html}");
    assert!(
        html.contains(
            r#"<a href="/p/Bob/" class="remarkup-mention" data-phid="PHID-USER-bob">@Bob</a>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="phabricator-remarkup-mention-unknown">@carol</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="https://bugzilla.allizom.org/show_bug.cgi?id=42">bug 42</a>"#),
        "{html}"
    );

    // Bob wrote D2, so he sees it.
    let as_bob = Viewer::user(Phid::new("PHID-USER-bob"), false);
    let html = phab_web::remarkup::render(&state, &as_bob, text, Mode::Html)
        .await
        .unwrap();
    assert!(
        String::from_utf8(html.into_bytes())
            .unwrap()
            .contains(r#"title="D2: Secret""#)
    );

    // Several texts resolve together, each keeping its own references.
    let rendered = phab_web::remarkup::render_all(
        &state,
        &as_alice,
        &["See D1.", "", "Ask @bob about #team.", "D2"],
        Mode::Html,
    )
    .await
    .unwrap();
    let rendered: Vec<String> = rendered
        .into_iter()
        .map(|markup| String::from_utf8(markup.into_bytes()).unwrap())
        .collect();
    assert_eq!(rendered.len(), 4);
    assert!(rendered[0].contains(r#"<a href="/D1""#), "{}", rendered[0]);
    assert!(!rendered[0].contains("@Bob"), "{}", rendered[0]);
    assert_eq!(rendered[1], "");
    assert!(
        rendered[2].contains(r#"data-phid="PHID-USER-bob">@Bob</a>"#),
        "{}",
        rendered[2]
    );
    assert!(!rendered[2].contains("/D1"), "{}", rendered[2]);
    assert_eq!(rendered[3], "<p>D2</p>");

    test_db.drop_databases().await;
}

/// A revision, files (visible, attached to the revision, hidden and
/// Alice's), Herald rules, repositories, commits and spaces.
const OTHER_OBJECTS: &str = "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-one', 'published', '', '', 'PHID-USER-bob', 1, 1, '{}', 'k', \
               'users', 'users', '{}', ''); \
             INSERT INTO `{ns}_file`.file (id, phid, name, mimeType, byteSize, storageEngine, storageFormat, \
               storageHandle, dateCreated, dateModified, authorPHID, secretKey, metadata, mailKey, viewPolicy) VALUES \
             (1, 'PHID-FILE-public', 'shot one.png', 'image/png', 2048, 'blob', 'raw', '1', 1, 1, \
               'PHID-USER-bob', 'secret1xxxxxxxxxxxxx', '{\"width\":440,\"height\":220}', 'k', 'users'), \
             (2, 'PHID-FILE-attached', 'log.txt', 'application/octet-stream', 10, 'blob', 'raw', '2', 1, 1, \
               'PHID-USER-bob', 'secret2xxxxxxxxxxxxx', '{}', 'k', 'no-one'), \
             (3, 'PHID-FILE-hidden', 'hidden.txt', 'text/plain', 10, 'blob', 'raw', '3', 1, 1, \
               'PHID-USER-bob', 'secret3xxxxxxxxxxxxx', '{}', 'k', 'no-one'), \
             (4, 'PHID-FILE-mine', 'mine.txt', 'text/plain', 10, 'blob', 'raw', '4', 1, 1, \
               'PHID-USER-alice', 'secret4xxxxxxxxxxxxx', '{}', 'k', 'no-one'); \
             INSERT INTO `{ns}_file`.file_attachment (objectPHID, filePHID, attacherPHID, attachmentMode, \
               dateCreated, dateModified) VALUES ('PHID-DREV-one', 'PHID-FILE-attached', NULL, 'attach', 1, 1); \
             INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, mustMatchAll, \
               dateCreated, dateModified, repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID) VALUES \
             (1, 'Security', 'PHID-USER-bob', 'differential', 1, 1, 1, 'every', 'global', 'PHID-HRUL-one', 0, NULL), \
             (2, 'Of a hidden repository', 'PHID-USER-bob', 'differential', 1, 1, 1, 'every', 'object', \
               'PHID-HRUL-two', 0, 'PHID-REPO-secret'); \
             INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, versionControlSystem, details, \
               dateCreated, dateModified, viewPolicy, editPolicy, pushPolicy, repositorySlug) VALUES \
             (1, 'PHID-REPO-mc', 'mozilla-central', 'MC', 'hg', '{\"tracking-enabled\":\"active\"}', 1, 1, \
               'users', 'users', 'users', 'mozilla-central'), \
             (2, 'PHID-REPO-secret', 'secret', 'SECRET', 'git', '{}', 1, 1, 'no-one', 'no-one', 'no-one', NULL); \
             INSERT INTO `{ns}_repository`.repository_commit (repositoryID, phid, commitIdentifier, epoch, \
               auditStatus, summary, importStatus) VALUES \
             (1, 'PHID-CMIT-one', 'abcdef0123456789abcdef0123456789abcdef01', 1, 'none', 'Land it', 15), \
             (2, 'PHID-CMIT-two', 'abcdef9999999999999999999999999999999999', 1, 'none', 'Hidden', 15); \
             INSERT INTO `{ns}_spaces`.spaces_namespace (id, phid, namespaceName, viewPolicy, editPolicy, \
               isDefaultNamespace, dateCreated, dateModified, description, isArchived) VALUES \
             (1, 'PHID-SPCE-default', 'Default', 'users', 'users', 1, 1, 1, '', 0), \
             (2, 'PHID-SPCE-secret', 'Secret', 'no-one', 'no-one', NULL, 1, 1, '', 0)";

#[tokio::test]
async fn files_rules_repositories_and_commits_resolve_under_policy() {
    let Some(test_db) = TestDatabase::create(&[
        "user",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
        "file",
        "herald",
    ])
    .await
    else {
        return;
    };
    test_db.execute(OTHER_OBJECTS).await;
    let config = Config::default();
    let state = State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        php: None,
        config,
    };
    let as_alice = Viewer::user(Phid::new("PHID-USER-alice"), false);
    let render = |text: &'static str| {
        let state = &state;
        let viewer = &as_alice;
        async move {
            let html = phab_web::remarkup::render(state, viewer, text, Mode::Html)
                .await
                .unwrap();
            String::from_utf8(html.into_bytes()).unwrap()
        }
    };

    let html = render("F1 F2 F3 F4 F5").await;
    for visible in ["F1", "F2", "F4"] {
        assert!(
            html.contains(&format!(">{visible}</a>")),
            "{visible}: {html}"
        );
    }
    assert!(html.contains("</a> F3 <a"), "{html}");
    assert!(html.ends_with("</a> F5</p>"), "{html}");

    let html = render("{F1}").await;
    // PHP swaps the width and the height in the default text.
    assert!(
        html.contains(r#"/file/xform/preview/PHID-FILE-public/secret1xxxxxxxxxxxxx/" width="220" height="110" alt="shot one.png (220×440 px, 2 KB)" />"#),
        "{html}"
    );
    assert!(
        html.contains("/file/data/secret1xxxxxxxxxxxxx/PHID-FILE-public/shot_one.png"),
        "{html}"
    );
    let html = render("{F2}").await;
    assert!(html.contains(r#"class="remarkup-file-link""#), "{html}");
    assert!(
        html.contains("/file/download/secret2xxxxxxxxxxxxx/PHID-FILE-attached/log.txt"),
        "{html}"
    );

    let html = render("H1 H2 rMC rSECRET R1 R2 S1 S2").await;
    assert!(html.contains(r#"title="H1 Security""#), "{html}");
    assert!(html.contains(" H2 "), "{html}");
    assert!(html.contains(r#"title="rMC mozilla-central""#), "{html}");
    assert!(
        html.contains(r#"<a href="/source/mozilla-central/""#),
        "{html}"
    );
    assert!(html.contains(" rSECRET "), "{html}");
    assert!(html.contains(">R1</a>"), "{html}");
    assert!(html.contains(" R2 "), "{html}");
    assert!(html.contains(r#"title="S1 Default""#), "{html}");
    assert!(html.ends_with(" S2</p>"), "{html}");

    let html = render("rMCabcdef0 and abcdef0123 and abcdef9999 and rSECRETabcdef9").await;
    assert!(
        html.contains(r#"<a href="/rMCabcdef0123456789abcdef0123456789abcdef01""#),
        "{html}"
    );
    assert!(
        html.contains(r#"title="rMCabcdef012345: Land it""#),
        "{html}"
    );
    assert!(
        html.contains("and abcdef9999 and rSECRETabcdef9</p>"),
        "{html}"
    );

    let html = render("{src mozilla-central:dom/a.cpp, line=3} {src secret:x}").await;
    assert!(
        html.contains(r#"href="/source/mozilla-central/browse/default/dom/a.cpp$3""#),
        "{html}"
    );
    assert!(html.ends_with(" {src secret:x}</p>"), "{html}");

    let html = render("{PHID-DREV-one} {PHID-REPO-secret} {PHID-FILE-mine}").await;
    assert!(html.contains(">D1: Open</a>"), "{html}");
    assert!(html.contains(" {PHID-REPO-secret} "), "{html}");
    assert!(html.contains(">F4: mine.txt</a>"), "{html}");

    test_db.drop_databases().await;
}
