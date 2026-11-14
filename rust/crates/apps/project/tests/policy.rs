//! Project visibility against a test database.

use phab_app_project::{policy, storage};
use phab_core::{Config, Phid};
use phab_db::testing::TestDatabase;
use phab_policy::Viewer;

/// A project fixture: PHID, view policy, parent, milestone number, slug.
type Fixture = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<i64>,
    Option<&'static str>,
);

const PROJECTS: &[Fixture] = &[
    ("PHID-PROJ-open", "users", None, None, Some("open")),
    ("PHID-PROJ-team", "no-one", None, None, Some("team")),
    (
        "PHID-PROJ-teamsub",
        "users",
        Some("PHID-PROJ-team"),
        None,
        Some("team-sub"),
    ),
    (
        "PHID-PROJ-teamms",
        "users",
        Some("PHID-PROJ-team"),
        Some(1),
        None,
    ),
    (
        "PHID-PROJ-orphan",
        "users",
        Some("PHID-PROJ-gone"),
        None,
        None,
    ),
    ("PHID-PROJ-outer", "no-one", None, None, None),
    (
        "PHID-PROJ-inner",
        "users",
        Some("PHID-PROJ-outer"),
        None,
        None,
    ),
];

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&["project", "user", "policy", "spaces"]).await?;
    let values: Vec<String> = PROJECTS
        .iter()
        .enumerate()
        .map(|(index, (phid, view, parent, milestone, slug))| {
            let sql = |value: Option<String>| value.map_or("NULL".to_owned(), |value| format!("'{value}'"));
            format!(
                "('{name}', '{phid}', 'PHID-USER-x', 1, 1, '0', '{view}', 'users', 'users', 'fa-x', 'blue', \
                  'k', 0, 0, 0, 'p{index}', 0, 'k{index:03}', '{{}}', 'default', {parent}, {milestone}, {slug})",
                name = phid.trim_start_matches("PHID-PROJ-"),
                parent = sql(parent.map(str::to_owned)),
                milestone = milestone.map_or("NULL".to_owned(), |number| number.to_string()),
                slug = sql(slug.map(str::to_owned)),
            )
        })
        .collect();
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_project`.project (name, phid, authorPHID, dateCreated, dateModified, status, \
               viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, hasMilestones, \
               hasSubprojects, projectPath, projectDepth, projectPathKey, properties, subtype, \
               parentProjectPHID, milestoneNumber, primarySlug) VALUES {}; \
             INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, dateModified) VALUES \
               ('PHID-PROJ-open', 'open', 1, 1), ('PHID-PROJ-open', 'also-open', 1, 1); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-PROJ-team', 60, 'PHID-USER-alice', 1, 0), \
               ('PHID-PROJ-inner', 60, 'PHID-USER-alice', 1, 0)",
            values.join(", ")
        ))
        .await;
    Some(test_db)
}

async fn visible(test_db: &TestDatabase, viewer: &str) -> Vec<String> {
    let phids: Vec<Phid> = PROJECTS.iter().map(|(phid, ..)| Phid::new(*phid)).collect();
    let projects = storage::load_by_phid(test_db.database(), &phids)
        .await
        .unwrap();
    let viewer = Viewer::user(Phid::new(viewer), false);
    let (projects, _) =
        policy::visible_projects(test_db.database(), &Config::default(), &viewer, projects, 1)
            .await
            .unwrap();
    let mut names: Vec<String> = projects.into_iter().map(|project| project.name).collect();
    names.sort();
    names
}

#[tokio::test]
async fn visibility_follows_parents_and_members() {
    let Some(test_db) = setup().await else {
        return;
    };
    // Alice is a member of "team", so she sees it, its subproject and its
    // milestone; as a member of "inner" she counts as a member of "outer".
    assert_eq!(
        visible(&test_db, "PHID-USER-alice").await,
        ["inner", "open", "outer", "team", "teamms", "teamsub"]
    );
    // Bob sees only the open project: the others need "team" or "outer".
    assert_eq!(visible(&test_db, "PHID-USER-bob").await, ["open"]);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn milestones_are_in_their_parents_space() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_spaces`.spaces_namespace (phid, namespaceName, viewPolicy, \
               editPolicy, isDefaultNamespace, dateCreated, dateModified, description, \
               isArchived) VALUES \
             ('PHID-SPCE-open', 'Open', 'users', 'users', 1, 1, 1, '', 0), \
             ('PHID-SPCE-hidden', 'Hidden', 'no-one', 'no-one', NULL, 1, 1, '', 0); \
             UPDATE `{ns}_project`.project SET spacePHID = 'PHID-SPCE-hidden' \
               WHERE phid = 'PHID-PROJ-teamms'",
        )
        .await;
    // The milestone's own column names a space Alice can't see, but its
    // parent "team" has none, so she still sees it.
    assert!(
        visible(&test_db, "PHID-USER-alice")
            .await
            .contains(&"teamms".to_owned())
    );

    test_db
        .execute(
            "UPDATE `{ns}_project`.project SET spacePHID = NULL \
               WHERE phid = 'PHID-PROJ-teamms'; \
             UPDATE `{ns}_project`.project SET spacePHID = 'PHID-SPCE-hidden' \
               WHERE phid = 'PHID-PROJ-team'",
        )
        .await;
    let seen = visible(&test_db, "PHID-USER-alice").await;
    assert!(!seen.contains(&"team".to_owned()), "{seen:?}");
    assert!(!seen.contains(&"teamms".to_owned()), "{seen:?}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn slugs_and_handles() {
    let Some(test_db) = setup().await else {
        return;
    };
    let found = storage::load_by_slug(
        test_db.database(),
        &["also-open".to_owned(), "nope".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(found.len(), 1);
    let (slug, project) = &found[0];
    assert_eq!(slug, "also-open");
    // The handle uses the primary hashtag.
    assert_eq!(project.uri(), "/tag/open/");

    let milestone = storage::load_by_phid(test_db.database(), &[Phid::new("PHID-PROJ-teamms")])
        .await
        .unwrap()
        .remove(0);
    let parent = storage::load_by_phid(test_db.database(), &[Phid::new("PHID-PROJ-team")])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(milestone.display_name(Some(&parent)), "team (teamms)");
    assert_eq!(milestone.uri(), format!("/project/view/{}/", milestone.id));
    test_db.drop_databases().await;
}
