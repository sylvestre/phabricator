//! `project.edit` and `project.create` against a test database.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-prprprprprprprprprprprprprpr";
const NOW: i64 = 1_700_000_000;

const DATABASES: &[&str] = &[
    "project", "user", "policy", "spaces", "conduit", "auth", "herald", "worker", "search",
];

async fn call(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_project::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, method, &http, Mode::Serve, NOW)
        .await
        .response
}

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn error_info(context: &Context, method: &str, params: Value) -> String {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info: Some(info),
            ..
        }) if code == "ERR-CONDUIT-CORE" => info,
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

/// The rows `sql` selects, as one `line` column each.
async fn lines(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| phab_db::rows::text(row, "line").expect("a line"))
        .collect()
}

/// The transactions of a project: type, old, new, metadata without the
/// group, and the columns every one shares.
async fn transactions(test_db: &TestDatabase, project: &str) -> Vec<String> {
    lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' ', transactionType, oldValue, newValue, \
               REPLACE(metadata, SUBSTRING(metadata, LOCATE('\"core.groupID\":\"', metadata) + 16, 32), 'G'), \
               authorPHID, viewPolicy, editPolicy, IFNULL(commentPHID, '-'), commentVersion, \
               contentSource, dateCreated, dateModified, LEFT(phid, 15)) AS line \
             FROM `{{ns}}_project`.project_transaction WHERE objectPHID = '{project}' ORDER BY id"
        ),
    )
    .await
}

async fn edges(test_db: &TestDatabase, database: &str) -> Vec<String> {
    lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' ', src, type, dst, dateCreated, seq, IFNULL(dataID, '-')) AS line \
             FROM `{{ns}}_{database}`.edge ORDER BY src, type, dst"
        ),
    )
    .await
}

async fn count(test_db: &TestDatabase, table: &str) -> usize {
    lines(
        test_db,
        &format!("SELECT CAST(COUNT(*) AS CHAR) AS line FROM {table}"),
    )
    .await[0]
        .parse()
        .expect("a count")
}

/// The data of the last publish task.
async fn task(test_db: &TestDatabase) -> Value {
    let data = lines(
        test_db,
        "SELECT CONCAT(t.taskClass, ' ', t.priority, ' ', t.objectPHID, ' ', d.data) AS line \
         FROM `{ns}_worker`.worker_activetask t JOIN `{ns}_worker`.worker_taskdata d \
           ON d.id = t.dataID ORDER BY t.id DESC LIMIT 1",
    )
    .await
    .remove(0);
    let (head, data) = data.split_at(data.find('{').expect("JSON"));
    let mut value: Value = serde_json::from_str(data).expect("JSON");
    value["head"] = json!(head.trim_end());
    value
}

/// The publish state PHP's project editor builds.
fn state(project: &str, header: &str, is_new: bool) -> Value {
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    json!({
        "parentMessageID": null,
        "isNewObject": is_new,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": header,
        "mailToPHIDs": ["PHID-USER-alice"],
        "mailCCPHIDs": [],
        "feedNotifyPHIDs": ["PHID-USER-alice"],
        "feedRelatedPHIDs": [project, "PHID-USER-alice"],
        "feedShouldPublish": true,
        "mailShouldSend": true,
        "mustEncrypt": [],
        "mailStamps": [
            stamp("string", "application", json!("Projects")),
            stamp("string", "phid", json!(project)),
            stamp("string", "object-type", json!("PROJ")),
            stamp("phid", "actor", json!("PHID-USER-alice")),
            stamp("string", "via", json!("conduit")),
            stamp("bool", "silent", Value::Null),
            stamp("bool", "encrypted", json!([])),
            stamp("bool", "new", json!(is_new)),
            stamp("phid", "mention", Value::Null),
            stamp("string", "herald", json!([])),
            stamp("phid", "removed", json!([])),
            stamp("phid", "space", Value::Null),
        ],
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": [],
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": [],
        "custom.encoding": [],
    })
}

fn phids(result: &Value) -> Vec<String> {
    result["transactions"]
        .as_array()
        .expect("transactions")
        .iter()
        .map(|xaction| xaction["phid"].as_str().expect("a PHID").to_owned())
        .collect()
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-carol', 'carol', 'Carol', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, isMembershipLocked, icon, \
               color, mailKey, primarySlug, hasWorkboard, hasMilestones, hasSubprojects, \
               projectPath, projectDepth, projectPathKey, properties, subtype) VALUES \
             (1, 'Build Reviewers', 'PHID-PROJ-build', 'PHID-USER-bob', 5, 5, '0', 'users', \
               'users', 'users', 0, 'project', 'blue', 'k', 'build_reviewers', 0, 0, 0, 'aaaa', 0, \
               'aaaa', '[]', 'default'), \
             (2, 'Locked Team', 'PHID-PROJ-locked', 'PHID-USER-bob', 5, 5, '0', 'users', \
               'admin', 'users', 0, 'group', 'red', 'k', 'locked_team', 0, 0, 0, 'bbbb', 0, 'bbbb', \
               '[]', 'default'), \
             (3, 'Hidden', 'PHID-PROJ-hidden', 'PHID-USER-bob', 5, 5, '0', 'admin', 'admin', \
               'admin', 0, 'policy', 'red', 'k', 'hidden', 0, 0, 0, 'cccc', 0, 'cccc', '[]', \
               'default'); \
             INSERT INTO `{{ns}}_project`.project_slug (id, projectPHID, slug, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-PROJ-build', 'build_reviewers', 5, 5), \
             (2, 'PHID-PROJ-locked', 'locked_team', 5, 5), \
             (3, 'PHID-PROJ-hidden', 'hidden', 5, 5); \
             INSERT INTO `{{ns}}_project`.project_datasourcetoken (projectID, token) VALUES \
             (1, 'build'), (1, 'reviewers'); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PROJ-build', 13, 'PHID-USER-bob', 5, 0), \
             ('PHID-PROJ-build', 13, 'PHID-USER-carol', 5, 1), \
             ('PHID-PROJ-build', 60, 'PHID-USER-bob', 5, 0), \
             ('PHID-PROJ-build', 60, 'PHID-USER-carol', 5, 1); \
             INSERT INTO `{{ns}}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-USER-bob', 14, 'PHID-PROJ-build', 5, 0), \
             ('PHID-USER-carol', 14, 'PHID-PROJ-build', 5, 0)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

const CONDUIT: &str = r#"{"source":"conduit","params":[]}"#;

#[tokio::test]
async fn creates_and_edits_projects_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // project.edit creates a project.
    let created = answer(
        &context,
        "project.edit",
        json!({"transactions": [
            {"type": "name", "value": "Release Team"},
            {"type": "members.add", "value": ["PHID-USER-bob", "PHID-USER-alice", "PHID-USER-bob"]},
            {"type": "icon", "value": "tag"},
            {"type": "color", "value": "blue"},
        ]}),
    )
    .await;
    assert_eq!(created["object"]["id"], 4);
    let project = created["object"]["phid"]
        .as_str()
        .expect("a PHID")
        .to_owned();
    assert!(project.starts_with("PHID-PROJ-"), "{project}");
    // The color was the default already.
    let xactions = phids(&created);
    assert_eq!(xactions.len(), 4);
    let row = |rest: &str| {
        format!("PHID-USER-alice public PHID-USER-alice - 0 {CONDUIT} {NOW} {NOW} {rest}")
    };
    assert_eq!(
        transactions(&test_db, &project).await,
        [
            format!(
                "core:create null null {{\"core.create\":true,\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
            format!(
                "project:name null \"Release Team\" {{\"core.create\":true,\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
            format!(
                "core:edge [] [\"PHID-USER-bob\",\"PHID-USER-alice\"] \
                 {{\"edge:type\":13,\"core.create\":true,\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
            format!(
                "project:icon \"project\" \"tag\" {{\"core.create\":true,\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
        ]
    );
    // One group for the edit.
    assert_eq!(
        lines(
            &test_db,
            &format!(
                "SELECT CAST(COUNT(DISTINCT SUBSTRING(metadata, \
                   LOCATE('\"core.groupID\":\"', metadata) + 16, 32)) AS CHAR) AS line FROM \
                 `{{ns}}_project`.project_transaction WHERE objectPHID = '{project}'"
            )
        )
        .await,
        ["1"]
    );
    let path_key = &phab_core::hash::digest_for_index(project.as_bytes())[..4];
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', id, name, authorPHID, dateCreated, dateModified, status, \
               viewPolicy, editPolicy, joinPolicy, isMembershipLocked, \
               IFNULL(profileImagePHID, '-'), icon, color, LENGTH(mailKey), primarySlug, \
               IFNULL(parentProjectPHID, '-'), hasWorkboard, hasMilestones, hasSubprojects, \
               IFNULL(milestoneNumber, '-'), projectPath, projectDepth, projectPathKey, \
               properties, IFNULL(spacePHID, '-'), subtype) AS line \
             FROM `{ns}_project`.project WHERE id = 4"
        )
        .await,
        [format!(
            "4 Release Team PHID-USER-alice {NOW} {NOW} 0 users users users 0 - tag blue 20 \
             release_team - 0 0 0 - {path_key} 0 {path_key} [] - default"
        )]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', projectID, token) AS line FROM \
             `{ns}_project`.project_datasourcetoken WHERE projectID = 4 ORDER BY id"
        )
        .await,
        ["4 release", "4 team"]
    );
    assert_eq!(
        lines(
            &test_db,
            &format!(
                "SELECT CONCAT_WS(' ', id, slug, dateCreated, dateModified) AS line FROM \
                 `{{ns}}_project`.project_slug WHERE projectPHID = '{project}' ORDER BY id"
            )
        )
        .await,
        [format!("4 release_team {NOW} {NOW}")]
    );
    let project_edges = edges(&test_db, "project").await;
    let mine: Vec<&String> = project_edges
        .iter()
        .filter(|line| line.starts_with(&project))
        .collect();
    assert_eq!(
        mine,
        [
            &format!("{project} 13 PHID-USER-alice {NOW} 1 -"),
            &format!("{project} 13 PHID-USER-bob {NOW} 0 -"),
            &format!("{project} 60 PHID-USER-alice {NOW} 1 -"),
            &format!("{project} 60 PHID-USER-bob {NOW} 0 -"),
        ]
    );
    let mut expected = vec![
        format!("PHID-USER-alice 14 {project} {NOW} 0 -"),
        "PHID-USER-bob 14 PHID-PROJ-build 5 0 -".to_owned(),
        format!("PHID-USER-bob 14 {project} {NOW} 0 -"),
        "PHID-USER-carol 14 PHID-PROJ-build 5 0 -".to_owned(),
    ];
    expected.sort();
    assert_eq!(edges(&test_db, "user").await, expected);
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', phid, header) AS line FROM `{ns}_herald`.herald_savedheader"
        )
        .await,
        [format!("{project} none")]
    );
    let transcript = lines(
        &test_db,
        "SELECT CONCAT_WS(' ', objectPHID, time, host, dryRun, garbageCollected, \
           CAST(objectTranscript AS CHAR), CAST(ruleTranscripts AS CHAR), \
           CAST(conditionTranscripts AS CHAR), CAST(applyTranscripts AS CHAR)) AS line \
         FROM `{ns}_herald`.herald_transcript",
    )
    .await
    .remove(0);
    assert!(
        transcript.starts_with(&format!(
            "{project} {NOW} web1 0 0 O:22:\"HeraldObjectTranscript\""
        )),
        "{transcript}"
    );
    assert!(
        transcript.contains("s:31:\"PhabricatorProjectHeraldAdapter\"")
            && transcript.contains("s:20:\"Project Release Team\"")
            && transcript.ends_with(" a:0:{} a:0:{} a:0:{}"),
        "{transcript}"
    );
    let published = task(&test_db).await;
    assert_eq!(
        published["head"],
        format!("PhabricatorApplicationTransactionPublishWorker 1000 {project}")
    );
    assert_eq!(published["objectPHID"], project);
    assert_eq!(published["actorPHID"], "PHID-USER-alice");
    assert_eq!(published["xactionPHIDs"], json!(xactions));
    assert_eq!(published["state"], state(&project, "none", true));

    // project.edit edits it: renamed, recolored, with other members and
    // hashtags.
    let edited = answer(
        &context,
        "project.edit",
        json!({"objectIdentifier": "PHID-PROJ-build", "transactions": [
            {"type": "name", "value": "Build Team"},
            {"type": "members.remove", "value": ["PHID-USER-carol"]},
            {"type": "slugs", "value": ["Builders", "builders"]},
            {"type": "color", "value": "red"},
            {"type": "icon", "value": "project"},
        ]}),
    )
    .await;
    assert_eq!(
        edited["object"],
        json!({"id": 1, "phid": "PHID-PROJ-build"})
    );
    // The icon didn't change.
    assert_eq!(phids(&edited).len(), 4);
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        [
            format!(
                "project:name \"Build Reviewers\" \"Build Team\" {{\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
            format!(
                "core:edge [\"PHID-USER-carol\"] [] {{\"edge:type\":13,\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
            format!(
                "project:slugs [] [\"builders\"] {{\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
            format!(
                "project:color \"blue\" \"red\" {{\"core.groupID\":\"G\"}} {}",
                row("PHID-XACT-PROJ-")
            ),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', name, primarySlug, icon, color, dateCreated, dateModified, \
               TRIM(TRAILING CHAR(0) FROM mailKey)) AS line FROM `{ns}_project`.project \
             WHERE id = 1"
        )
        .await,
        [format!("Build Team build_team project red 5 {NOW} k")]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', id, slug) AS line FROM `{ns}_project`.project_slug \
             WHERE projectPHID = 'PHID-PROJ-build' ORDER BY id"
        )
        .await,
        ["1 build_reviewers", "5 build_team", "6 builders"]
    );
    // The tokens of the hashtags the project had when it was saved.
    assert_eq!(
        lines(
            &test_db,
            "SELECT token AS line FROM `{ns}_project`.project_datasourcetoken \
             WHERE projectID = 1 ORDER BY id"
        )
        .await,
        ["build_reviewers", "build", "team"]
    );
    let project_edges = edges(&test_db, "project").await;
    let build: Vec<&String> = project_edges
        .iter()
        .filter(|line| line.starts_with("PHID-PROJ-build"))
        .collect();
    assert_eq!(
        build,
        [
            "PHID-PROJ-build 13 PHID-USER-bob 5 0 -",
            "PHID-PROJ-build 60 PHID-USER-bob 5 0 -",
        ]
    );
    assert!(
        !edges(&test_db, "user")
            .await
            .iter()
            .any(|line| line.starts_with("PHID-USER-carol"))
    );
    let published = task(&test_db).await;
    assert_eq!(published["xactionPHIDs"], json!(phids(&edited)));
    assert_eq!(published["state"], state("PHID-PROJ-build", "none", false));

    // Joining a project one may not edit only needs to be able to join.
    let joined = answer(
        &context,
        "project.edit",
        json!({"objectIdentifier": 2, "transactions": [
            {"type": "members.add", "value": ["PHID-USER-alice"]},
        ]}),
    )
    .await;
    assert_eq!(phids(&joined).len(), 1);
    assert!(
        edges(&test_db, "project")
            .await
            .contains(&format!("PHID-PROJ-locked 60 PHID-USER-alice {NOW} 0 -"))
    );

    // project.create: no core:create, and the members and slugs of the
    // object in memory.
    let legacy = answer(
        &context,
        "project.create",
        json!({"name": "Docs", "members": ["PHID-USER-carol"], "icon": "", "color": "green",
               "tags": ["documentation", "docs"]}),
    )
    .await;
    let docs = legacy["phid"].as_str().expect("a PHID").to_owned();
    assert_eq!(
        legacy,
        json!({
            "id": 5,
            "phid": docs,
            "name": "Docs",
            "profileImagePHID": null,
            "icon": "project",
            "color": "green",
            "members": [],
            "slugs": [],
            "dateCreated": NOW,
            "dateModified": NOW,
        })
    );
    assert_eq!(
        transactions(&test_db, &docs)
            .await
            .iter()
            .map(|line| line.split(" {").next().unwrap_or_default().to_owned())
            .collect::<Vec<_>>(),
        [
            "project:name null \"Docs\"",
            "project:color \"blue\" \"green\"",
            "project:slugs [] [\"documentation\",\"docs\"]",
            "core:edge [] [\"PHID-USER-carol\"]",
        ]
    );
    // The name's hashtag first; the slugs transaction finds "docs" there.
    assert_eq!(
        lines(
            &test_db,
            &format!(
                "SELECT slug AS line FROM `{{ns}}_project`.project_slug WHERE projectPHID = \
                 '{docs}' ORDER BY id"
            )
        )
        .await,
        ["docs", "documentation"]
    );

    // Validation errors.
    let create = |name: &str| json!({"transactions": [{"type": "name", "value": name}]});
    assert_eq!(
        error_info(&context, "project.edit", create("Build  team")).await,
        "Validation errors:\n  - Project name generates the same hashtag (\"#build_team\") as \
         another existing project. Choose a unique name."
    );
    assert_eq!(
        error_info(&context, "project.edit", create("!!!")).await,
        "Validation errors:\n  - Project names must contain at least one letter or number."
    );
    assert_eq!(
        error_info(&context, "project.edit", create("")).await,
        "Validation errors:\n  - Projects must have a name."
    );
    assert_eq!(
        error_info(
            &context,
            "project.edit",
            json!({"transactions": [{"type": "icon", "value": "tag"}]})
        )
        .await,
        "Validation errors:\n  - Projects must have a name."
    );
    assert_eq!(
        error_info(
            &context,
            "project.create",
            json!({"name": "Release_Team", "members": []})
        )
        .await,
        "Validation errors:\n  - Project name generates the same hashtag (\"#release_team\") as \
         another existing project. Choose a unique name."
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let edit = |identifier: Value, transactions: Value| json!({"objectIdentifier": identifier, "transactions": transactions});
    for (method, params) in [
        // Custom fields, policies, unknown types.
        (
            "project.edit",
            edit(json!(1), json!([{"type": "description", "value": "Hi"}])),
        ),
        (
            "project.edit",
            edit(json!(1), json!([{"type": "view", "value": "users"}])),
        ),
        // Projects the viewer can't see, or edit.
        (
            "project.edit",
            edit(json!(3), json!([{"type": "name", "value": "Seen"}])),
        ),
        (
            "project.edit",
            edit(json!(2), json!([{"type": "color", "value": "blue"}])),
        ),
        (
            "project.edit",
            edit(
                json!("#build_reviewers"),
                json!([{"type": "color", "value": "red"}]),
            ),
        ),
        (
            "project.edit",
            edit(json!(99), json!([{"type": "color", "value": "red"}])),
        ),
        // Edits without effect, empty names on edits.
        (
            "project.edit",
            edit(json!(1), json!([{"type": "color", "value": "blue"}])),
        ),
        (
            "project.edit",
            edit(json!(1), json!([{"type": "name", "value": ""}])),
        ),
        ("project.edit", edit(json!(1), json!([]))),
        // Usernames, unknown users, other objects.
        (
            "project.edit",
            edit(
                json!(1),
                json!([{"type": "members.add", "value": ["alice"]}]),
            ),
        ),
        (
            "project.edit",
            edit(
                json!(1),
                json!([{"type": "members.add", "value": ["PHID-USER-nobody"]}]),
            ),
        ),
        (
            "project.edit",
            edit(
                json!(1),
                json!([{"type": "members.add", "value": ["PHID-PROJ-locked"]}]),
            ),
        ),
        // Hashtags of others, invalid ones, non-ASCII names.
        (
            "project.edit",
            edit(json!(1), json!([{"type": "slugs", "value": ["hidden"]}])),
        ),
        (
            "project.edit",
            edit(json!(1), json!([{"type": "slugs", "value": ["??"]}])),
        ),
        (
            "project.edit",
            json!({"transactions": [{"type": "name", "value": "Équipe"}]}),
        ),
        // Several transactions of one type, values of other types.
        (
            "project.edit",
            edit(
                json!(1),
                json!([{"type": "color", "value": "red"}, {"type": "color", "value": "green"}]),
            ),
        ),
        (
            "project.edit",
            edit(json!(1), json!([{"type": "color", "value": 5}])),
        ),
        // project.create without members, or with odd values.
        ("project.create", json!({"name": "Solo"})),
        (
            "project.create",
            json!({"name": "Solo", "members": [], "icon": 5}),
        ),
        (
            "project.create",
            json!({"name": "Solo", "members": [], "tags": "solo"}),
        ),
    ] {
        assert!(
            matches!(
                call(&context, method, params.clone()).await,
                Response::Fallback(_)
            ),
            "{method} {params}"
        );
    }
    // Herald rules about projects.
    test_db
        .execute(
            "INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, \
               ruleType, phid, isDisabled, triggerObjectPHID) VALUES \
             (1, 'Projects', 'PHID-USER-bob', 'PhabricatorProjectHeraldAdapter', 1, 38, 1, 1, \
               'every', 'global', 'PHID-HRUL-one', 0, NULL)",
        )
        .await;
    assert!(matches!(
        call(
            &context,
            "project.edit",
            edit(json!(1), json!([{"type": "color", "value": "red"}]))
        )
        .await,
        Response::Fallback(_)
    ));

    // Nothing was written.
    let namespace = test_db.database().namespace().to_owned();
    for table in [
        "project_transaction",
        "project",
        "project_slug",
        "project_datasourcetoken",
        "edge",
    ] {
        let expected = match table {
            "project" | "project_slug" => 3,
            "project_datasourcetoken" => 2,
            "edge" => 4,
            _ => 0,
        };
        assert_eq!(
            count(&test_db, &format!("`{namespace}_project`.{table}")).await,
            expected,
            "{table}"
        );
    }
    assert_eq!(
        count(&test_db, &format!("`{namespace}_worker`.worker_activetask")).await,
        0
    );
    assert_eq!(
        count(&test_db, &format!("`{namespace}_herald`.herald_transcript")).await,
        0
    );

    test_db.drop_databases().await;
}
