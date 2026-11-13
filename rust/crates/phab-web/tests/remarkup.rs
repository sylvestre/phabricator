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

    test_db.drop_databases().await;
}
