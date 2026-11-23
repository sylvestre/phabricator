//! The Harbormaster pages against one test database.
//!
//! Alice and Bob see the buildable of D1's diff (B1) and the manual one of
//! a diff without revision (B3); only Alice sees D2 and its buildable (B2).
//! Build 1 runs the "Try" plan, which everyone sees and only
//! administrators (Root) edit; build 2 runs the "Secret" plan nobody sees.

// The checks share one database, so each page's are one long function.
#![allow(clippy::too_many_lines)]

use std::io::Write;
use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

const ALICE: &str = "phsid=alice-token";
const BOB: &str = "phsid=bob-token";
const ROOT: &str = "phsid=root-token";

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

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

/// The log of target 1: thirty numbered lines, the last ten in a chunk
/// compressed like `gzdeflate()`.
fn log_chunks() -> (String, String) {
    let mut first = (1..=20)
        .map(|line| format!("line{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    first.push('\n');
    let rest: String = (21..=30)
        .map(|line| format!("line{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(rest.as_bytes()).unwrap();
    (first, hex::encode(encoder.finish().unwrap()))
}

async fn setup() -> Option<TestDatabase> {
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
        "herald",
        "search",
    ])
    .await?;
    let (first_chunk, gzip_chunk) = log_chunks();
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0), \
             ('PHID-USER-gone', 'gone', 'Gone', 1, 1, 'c', 0, 1, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-bob', 'web', '{bob}', 1, {expires}, 'PHID-SSSN-2', 0), \
             ('PHID-USER-root', 'web', '{root}', 1, {expires}, 'PHID-SSSN-3', 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', '{{}}', 'PHID-DIFF-1'), \
             (2, 'Hidden', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'no-one', 'no-one', '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (3, 'PHID-DIFF-3', NULL, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_project`.project (name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             ('CI', 'PHID-PROJ-ci', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', 'red', 'k', \
               0, 0, 0, 'a', 0, 'k001', '{{}}', 'default', 'ci'), \
             ('Hush', 'PHID-PROJ-hush', 'x', 1, 1, '0', 'no-one', 'users', 'users', 'fa-x', 'red', \
               'k', 0, 0, 0, 'b', 0, 'k002', '{{}}', 'default', 'hush'); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
               containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) VALUES \
             (1, 'PHID-HMBB-1', 'PHID-DIFF-1', 'PHID-DREV-1', 'passed', 10, 11, 0), \
             (2, 'PHID-HMBB-2', 'PHID-DIFF-2', 'PHID-DREV-2', 'failed', 20, 21, 0), \
             (3, 'PHID-HMBB-3', 'PHID-DIFF-3', NULL, 'building', 30, 31, 1); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
               dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
             (1, 'PHID-HMCP-1', 'Try', 'active', 100, 101, NULL, 'users', 'admin', \
               '{{\"behavior.hold-drafts\":\"never\"}}'), \
             (2, 'PHID-HMCP-2', 'Secret', 'active', 200, 201, NULL, 'no-one', 'no-one', '[]'), \
             (3, 'PHID-HMCP-3', 'Old', 'disabled', 300, 301, NULL, 'users', 'users', '[]'), \
             (4, 'PHID-HMCP-4', 'Empty', 'active', 400, 401, NULL, 'users', 'users', '[]'); \
             INSERT INTO `{{ns}}_harbormaster`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-HMCP-1', 41, 'PHID-PROJ-ci', 2, 0), ('PHID-HMCP-1', 41, 'PHID-PROJ-hush', 1, 0); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildstep (id, phid, buildPlanPHID, \
               className, details, dateCreated, dateModified, sequence, name, description, \
               stepAutoKey) VALUES \
             (1, 'PHID-HMCS-1', 'PHID-HMCP-1', 'HarbormasterHTTPRequestBuildStepImplementation', \
               '{{\"uri\":\"https://ci.example/run\",\"method\":\"GET\"}}', 1, 1, 0, 'Ping CI', \
               'Pings **CI**.', NULL), \
             (2, 'PHID-HMCS-2', 'PHID-HMCP-1', \
               'HarbormasterAbortOlderBuildsBuildStepImplementation', '{{}}', 1, 1, 0, NULL, '', \
               NULL); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_build (id, phid, buildablePHID, \
               buildPlanPHID, buildStatus, dateCreated, dateModified, buildGeneration, \
               planAutoKey, buildParameters, initiatorPHID) VALUES \
             (1, 'PHID-HMBD-1', 'PHID-HMBB-1', 'PHID-HMCP-1', 'passed', 1000, 1001, 2, NULL, \
               '{{}}', 'PHID-USER-alice'), \
             (2, 'PHID-HMBD-2', 'PHID-HMBB-1', 'PHID-HMCP-2', 'building', 2000, 2001, 0, NULL, \
               '{{}}', NULL), \
             (3, 'PHID-HMBD-3', 'PHID-HMBB-2', 'PHID-HMCP-1', 'failed', 3000, 3001, 0, NULL, \
               '{{}}', NULL), \
             (4, 'PHID-HMBD-4', 'PHID-HMBB-3', 'PHID-HMCP-1', 'building', 4000, 4001, 0, NULL, \
               '{{}}', NULL); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildtarget (id, phid, buildPHID, \
               buildStepPHID, className, details, variables, dateCreated, dateModified, \
               targetStatus, name, dateStarted, dateCompleted, buildGeneration) VALUES \
             (1, 'PHID-HMBT-1', 'PHID-HMBD-1', 'PHID-HMCS-1', \
               'HarbormasterHTTPRequestBuildStepImplementation', \
               '{{\"uri\":\"https://ci.example/run\",\"dependsOn\":[],\"retries\":[2,null,true]}}', \
               '{{\"buildable.diff\":\"1\",\"buildable.commit\":null,\"step.timestamp\":1.5}}', \
               {created}, 2, 'target/passed', 'Ping CI', {started}, {completed}, 2), \
             (2, 'PHID-HMBT-2', 'PHID-HMBD-1', 'PHID-HMCS-1', \
               'HarbormasterHTTPRequestBuildStepImplementation', '{{}}', '{{}}', {created}, 2, \
               'target/failed', 'Ping CI', NULL, NULL, 1), \
             (3, 'PHID-HMBT-3', 'PHID-HMBD-2', 'PHID-HMCS-9', \
               'HarbormasterHTTPRequestBuildStepImplementation', '{{}}', '{{}}', {created}, 2, \
               'target/building', 'Secret step', {started}, NULL, 0), \
             (4, 'PHID-HMBT-4', 'PHID-HMBD-4', 'PHID-HMCS-1', \
               'HarbormasterHTTPRequestBuildStepImplementation', '{{}}', '{{}}', {created}, 2, \
               'target/building', 'Ping CI', {started}, NULL, 0); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildartifact (id, phid, artifactType, \
               artifactIndex, artifactKey, artifactData, dateCreated, dateModified, \
               buildTargetPHID, isReleased) VALUES \
             (1, 'PHID-HMBA-1', 'uri', 'aaaaaaaaaaaa', 'treeherder', \
               '{{\"uri\":\"https://treeherder.example/1\",\"name\":\"Treeherder\",\"ui.external\":true}}', \
               7, 8, 'PHID-HMBT-1', 0), \
             (2, 'PHID-HMBA-2', 'uri', 'bbbbbbbbbbbb', 'bad', \
               '{{\"uri\":\"javascript-ish/x\",\"ui.external\":false}}', 7, 8, 'PHID-HMBT-1', 0); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildlog (id, phid, logSource, logType, \
               duration, live, dateCreated, dateModified, buildTargetPHID, filePHID, byteLength, \
               chunkFormat, lineMap) VALUES \
             (1, 'PHID-HMCL-1', 'remote', 'stdout', NULL, 0, 9, 10, 'PHID-HMBT-1', NULL, 100, \
               'text', '[]'), \
             (2, 'PHID-HMCL-2', 'remote', 'stderr', NULL, 0, 9, 10, 'PHID-HMBT-1', NULL, 0, \
               'text', '[]'); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildlogchunk (logID, encoding, size, \
               chunk, headOffset, tailOffset) VALUES \
             (1, 'text', 1, '{first_chunk}', 0, 1), (1, 'gzip', 1, UNHEX('{gzip_chunk}'), 1, 2), \
             (2, 'text', 1, '  ', 0, 1); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildmessage (id, authorPHID, \
               receiverPHID, type, isConsumed, dateCreated, dateModified) VALUES \
             (1, 'PHID-USER-alice', 'PHID-HMBT-1', 'pass', 1, 50, 50); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildlintmessage (buildTargetPHID, \
               path, line, characterOffset, code, severity, name, properties, dateCreated, \
               dateModified) VALUES \
             ('PHID-HMBT-1', 'b.c', 7, 0, 'W1', 'warning', 'Meh', '{{}}', 1, 1), \
             ('PHID-HMBT-1', 'a.c', 3, 0, 'E1', 'error', 'Bad <thing>', '{{}}', 1, 1); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildunitmessage (buildTargetPHID, \
               engine, namespace, name, result, duration, properties, dateCreated, dateModified, \
               nameIndex) VALUES \
             ('PHID-HMBT-4', '', '', 'test', 'pass', NULL, '{{}}', 1, 1, 'iiiiiiiiiiii'); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildabletransaction (phid, \
               authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, \
               transactionType, oldValue, newValue, contentSource, metadata, dateCreated, \
               dateModified) VALUES \
             ('PHID-XACT-HMBB-1', 'PHID-USER-alice', 'PHID-HMBB-1', 'users', 'users', NULL, 0, \
               'harbormaster:buildable:command', 'null', '\"restart\"', '{{}}', '{{}}', 60, 60), \
             ('PHID-XACT-HMBB-2', 'PHID-USER-bob', 'PHID-HMBB-1', 'PHID-USER-bob', 'users', \
               NULL, 0, 'harbormaster:buildable:command', 'null', '\"abort\"', '{{}}', '{{}}', 61, \
               61); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildtransaction (phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-HMBD-1', 'PHID-USER-root', 'PHID-HMBD-1', 'users', 'users', NULL, 0, \
               'message/restart', 'null', 'null', '{{}}', '{{}}', 70, 70); \
             INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildplantransaction (phid, \
               authorPHID, objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, \
               transactionType, oldValue, newValue, contentSource, metadata, dateCreated, \
               dateModified) VALUES \
             ('PHID-XACT-HMCP-1', 'PHID-USER-root', 'PHID-HMCP-1', 'users', 'users', NULL, 0, \
               'core:create', 'null', 'null', '{{}}', '{{\"core.create\":true}}', 80, 80), \
             ('PHID-XACT-HMCP-2', 'PHID-USER-root', 'PHID-HMCP-1', 'users', 'users', NULL, 0, \
               'harbormaster:name', '\"\"', '\"Try\"', '{{}}', '{{\"core.create\":true}}', 80, 80), \
             ('PHID-XACT-HMCP-3', 'PHID-USER-root', 'PHID-HMCP-1', 'users', 'users', NULL, 0, \
               'core:view-policy', '\"admin\"', '\"users\"', '{{}}', '{{\"core.create\":true, \
               \"core.default\":true}}', 80, 80), \
             ('PHID-XACT-HMCP-4', 'PHID-USER-root', 'PHID-HMCP-1', 'users', 'users', NULL, 0, \
               'behavior', '\"always\"', '\"never\"', '{{}}', \
               '{{\"behavior-key\":\"hold-drafts\"}}', 90, 90); \
             INSERT INTO `{{ns}}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, dateCreated, dateModified, repetitionPolicy, ruleType, phid, \
               isDisabled) VALUES \
             (1, 'Run Try', 'PHID-USER-root', 'differential', 1, 1, 1, 'every', 'global', \
               'PHID-HRUL-1', 0), \
             (2, 'Mine', 'PHID-USER-gone', 'differential.diff', 1, 1, 1, 'every', 'personal', \
               'PHID-HRUL-2', 0), \
             (3, 'Off', 'PHID-USER-root', 'differential', 1, 1, 1, 'every', 'global', \
               'PHID-HRUL-3', 1); \
             INSERT INTO `{{ns}}_herald`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-HRUL-1', 69, 'PHID-HMCP-1', 1, 0), ('PHID-HRUL-2', 69, 'PHID-HMCP-1', 1, 0), \
             ('PHID-HRUL-3', 69, 'PHID-HMCP-1', 1, 0); \
             INSERT INTO `{{ns}}_search`.search_namedquery (userPHID, engineClassName, queryName, \
               queryKey, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'HarbormasterBuildPlanSearchEngine', 'Mine', 'abc', 1, 1)",
            alice = session_key("alice-token"),
            bob = session_key("bob-token"),
            root = session_key("root-token"),
            expires = NOW + 29 * 86_400,
            created = NOW - 600,
            started = NOW - 300,
            completed = NOW - 100,
        ))
        .await;
    test_db.execute(CHECKS).await;
    Some(test_db)
}

/// Unit results of target 1 (six, so the summary shows five), named
/// through the string table; the file artifact; the plan whose steps
/// depend on each other; D1 as a draft; D2's lint and unit statuses; D3,
/// whose diff has legacy lint messages.
const CHECKS: &str = "\
    INSERT INTO `{ns}_harbormaster`.harbormaster_string (stringIndex, stringValue) VALUES \
    ('nnnnnnnnnnn1', 'test_fails'), ('nnnnnnnnnnn2', 'test_passes'), \
    ('nnnnnnnnnnn3', 'test_breaks'), ('nnnnnnnnnnn4', 'test_skips'), \
    ('nnnnnnnnnnn5', 'test_more'), ('nnnnnnnnnnn6', 'test_last'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildunitmessage (id, buildTargetPHID, engine, \
      namespace, name, result, duration, properties, dateCreated, dateModified, nameIndex) \
    VALUES \
    (11, 'PHID-HMBT-1', 'py', 'Suite', 'x', 'fail', NULL, \
      '{\"details\":\"one\\\\ntwo <b>\\\\nthree\\\\nfour\"}', 1, 1, 'nnnnnnnnnnn1'), \
    (12, 'PHID-HMBT-1', '', '', 'x', 'pass', 1.2345, '{}', 1, 1, 'nnnnnnnnnnn2'), \
    (13, 'PHID-HMBT-1', '', '', 'x', 'broken', NULL, \
      '{\"details\":\"**loud**\",\"format\":\"remarkup\"}', 1, 1, 'nnnnnnnnnnn3'), \
    (14, 'PHID-HMBT-1', '', '', 'x', 'skip', NULL, '{}', 1, 1, 'nnnnnnnnnnn4'), \
    (15, 'PHID-HMBT-1', '', '', 'x', 'pass', NULL, '{}', 1, 1, 'nnnnnnnnnnn5'), \
    (16, 'PHID-HMBT-1', '', '', 'x', 'pass', NULL, '{}', 1, 1, 'nnnnnnnnnnn6'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildartifact (id, phid, artifactType, \
      artifactIndex, artifactKey, artifactData, dateCreated, dateModified, buildTargetPHID, \
      isReleased) VALUES \
    (3, 'PHID-HMBA-3', 'file', 'cccccccccccc', 'zlog', '{\"filePHID\":\"PHID-FILE-1\"}', 7, 8, \
      'PHID-HMBT-1', 0), \
    (4, 'PHID-HMBA-4', 'file', 'dddddddddddd', 'zsecret', '{\"filePHID\":\"PHID-FILE-2\"}', 7, \
      8, 'PHID-HMBT-1', 0); \
    INSERT INTO `{ns}_file`.file (id, phid, name, mimeType, byteSize, storageEngine, \
      storageFormat, storageHandle, dateCreated, dateModified, authorPHID, secretKey, metadata, \
      mailKey, viewPolicy) VALUES \
    (1, 'PHID-FILE-1', 'build.log', 'text/plain', 10, 'blob', 'raw', '1', 1, 1, \
      'PHID-USER-root', 'secret1xxxxxxxxxxxxx', '{}', 'k', 'users'), \
    (2, 'PHID-FILE-2', 'secret.log', 'text/plain', 10, 'blob', 'raw', '2', 1, 1, \
      'PHID-USER-root', 'secret2xxxxxxxxxxxxx', '{}', 'k', 'no-one'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
      dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
    (5, 'PHID-HMCP-5', 'Graph', 'active', 500, 501, NULL, 'users', 'users', '[]'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildstep (id, phid, buildPlanPHID, \
      className, details, dateCreated, dateModified, sequence, name, description, \
      stepAutoKey) VALUES \
    (10, 'PHID-HMCS-10', 'PHID-HMCP-5', 'HarbormasterHTTPRequestBuildStepImplementation', \
      '{}', 1, 1, 0, 'First', '', NULL), \
    (11, 'PHID-HMCS-11', 'PHID-HMCP-5', 'HarbormasterHTTPRequestBuildStepImplementation', \
      '{\"dependsOn\":[\"PHID-HMCS-10\"]}', 1, 1, 0, 'Second', '', NULL), \
    (12, 'PHID-HMCS-12', 'PHID-HMCP-5', 'HarbormasterHTTPRequestBuildStepImplementation', \
      '{\"dependsOn\":[\"PHID-HMCS-gone\"]}', 1, 1, 0, 'Stuck', '', NULL); \
    UPDATE `{ns}_differential`.differential_revision SET properties = \
      '{\"draft.broadcast\":false}' WHERE id = 1; \
    UPDATE `{ns}_differential`.differential_diff SET lintStatus = 3, unitStatus = 4 \
      WHERE id = 2; \
    INSERT INTO `{ns}_differential`.differential_changeset (id, phid, diffID, oldFile, \
      filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
      addLines, delLines, dateCreated, dateModified) VALUES \
    (10, 'PHID-DCHG-ten', 1, NULL, 'a.c', '[]', 2, 1, '{}', '[]', '[]', 0, 0, 1, 1); \
    INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, summary, \
      testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
      editPolicy, properties, activeDiffPHID) VALUES \
    (3, 'Legacy', 'PHID-DREV-3', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
      'users', 'users', '{}', 'PHID-DIFF-4'); \
    INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, authorPHID, \
      lintStatus, unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
    (4, 'PHID-DIFF-4', 3, 'PHID-USER-alice', 2, 0, 1, 1, 1, 'users'); \
    INSERT INTO `{ns}_differential`.differential_diffproperty (diffID, name, data, \
      dateCreated, dateModified) VALUES (4, 'arc:lint', '[{\"code\":\"X\"}]', 1, 1)";

async fn get(test_db: &TestDatabase, path: &str, cookie: Option<&str>) -> Response {
    let config = Config::default();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn page(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, Some(cookie)).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{path} as {cookie}: {reason:?} {text}"
    );
    text
}

async fn status(test_db: &TestDatabase, path: &str, cookie: &str) -> StatusCode {
    get(test_db, path, Some(cookie)).await.status()
}

async fn fallback(test_db: &TestDatabase, path: &str, cookie: Option<&str>) -> String {
    let response = get(test_db, path, cookie).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn buildables(test_db: &TestDatabase) {
    let page = page(test_db, "/B1", ALICE).await;
    assert!(page.contains("Buildable 1</h1>"), "{page}");
    assert!(page.contains("Passed"), "{page}");
    assert!(
        page.contains("<dt>Container</dt><dd><a href=\"/D1\">D1: Open</a></dd>"),
        "{page}"
    );
    assert!(
        page.contains("<dd><a href=\"/differential/diff/1/\">Diff 1</a></dd>"),
        "{page}"
    );
    assert!(page.contains("Automatic Buildable"), "{page}");
    // Lint, worst first.
    let error = page.find("Bad &#60;thing&#62;").expect("the error");
    let warning = page.find("Meh").expect("the warning");
    assert!(error < warning, "{page}");
    assert!(page.contains("<td class=\"hm-pri\">a.c:3</td>"), "{page}");
    // The builds: the secret plan's build has no name.
    assert!(
        page.contains("Build 1</span> <a href=\"/harbormaster/build/1/\">Try</a>"),
        "{page}"
    );
    assert!(
        page.contains("Build 2</span> <a href=\"/harbormaster/build/2/\">Build</a>"),
        "{page}"
    );
    assert!(!page.contains(">Secret<"), "{page}");
    // Only the targets of the current run.
    assert!(
        page.contains("Target 1 <span class=\"muted\">Ping CI</span>"),
        "{page}"
    );
    assert!(!page.contains("Target 2 "), "{page}");
    assert!(
        page.contains("Target 3 <span class=\"muted\">Secret step</span>"),
        "{page}"
    );
    // Alice sees her command, not Bob's.
    assert!(
        page.contains("<a href=\"/p/alice/\">alice</a> restarted this buildable."),
        "{page}"
    );
    assert!(!page.contains("aborted this buildable"), "{page}");
    // Nobody edits the "Try" plan but Root, and no one sees "Secret".
    assert!(page.contains("hm-disabled"), "{page}");
    assert!(
        !page.contains("/harbormaster/buildable/1/restart/"),
        "{page}"
    );

    let page = page_as(test_db, "/B1", BOB).await;
    assert!(
        page.contains("<a href=\"/p/bob/\">bob</a> aborted this buildable."),
        "{page}"
    );

    let page = page_as(test_db, "/B1", ROOT).await;
    assert!(
        page.contains("<a href=\"/harbormaster/buildable/1/restart/\">"),
        "{page}"
    );
    // Build 1 passed: it can't be paused or aborted.
    assert!(!page.contains("/harbormaster/buildable/1/pause/"), "{page}");

    // Bob can't see D2, nor its buildable.
    assert_eq!(status(test_db, "/B2", BOB).await, StatusCode::NOT_FOUND);
    let page = page_as(test_db, "/B2", ALICE).await;
    assert!(page.contains("D2: Hidden"), "{page}");
    assert_eq!(status(test_db, "/B9", ALICE).await, StatusCode::NOT_FOUND);
    assert_eq!(status(test_db, "/B01", ALICE).await, StatusCode::NOT_FOUND);

    // A result whose name is not in the string table.
    let page = page_as(test_db, "/B3", ALICE).await;
    assert!(
        page.contains("Unknown Unit Message (&#34;iiiiiiiiiiii&#34;)"),
        "{page}"
    );
    assert_eq!(fallback(test_db, "/B1", None).await, "logged out");
}

async fn page_as(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    page(test_db, path, cookie).await
}

async fn builds(test_db: &TestDatabase) {
    let page = page_as(test_db, "/harbormaster/build/1/", ALICE).await;
    assert!(page.contains("Build 1</h1>"), "{page}");
    assert!(
        page.contains("<dd><a href=\"/B1\">B1: Diff 1</a></dd>"),
        "{page}"
    );
    assert!(
        page.contains("<dd><a href=\"/harbormaster/plan/1/\">Plan 1 Try</a></dd>"),
        "{page}"
    );
    // Runs 2 and 1; the current one is shown.
    assert!(page.contains("Run 2</a>"), "{page}");
    assert!(page.contains("Run 1</a>"), "{page}");
    assert!(!page.contains("older run"), "{page}");
    assert!(
        page.contains(
            "<a href=\"https://treeherder.example/1\" target=\"_blank\" \
             rel=\"noreferrer\">Treeherder</a>"
        ),
        "{page}"
    );
    assert!(page.contains("&lt;Invalid URI&gt;"), "{page}");
    assert!(page.contains("Completed at 2023-11-14 22:11 UTC"), "{page}");
    assert!(page.contains("Built for 3 m, 20 s"), "{page}");
    assert!(page.contains("Pings <strong>CI</strong>."), "{page}");
    assert!(
        page.contains("<dt>uri</dt><dd>https://ci.example/run</dd>"),
        "{page}"
    );
    assert!(
        page.contains("<td class=\"hm-pri\">buildable.diff</td><td>1</td>"),
        "{page}"
    );
    // Lists and nulls, as `phutil_escape_html()` renders them: the
    // values one after the other, and nothing.
    assert!(page.contains("<dt>dependsOn</dt><dd></dd>"), "{page}");
    assert!(page.contains("<dt>retries</dt><dd>21</dd>"), "{page}");
    assert!(
        page.contains("<td class=\"hm-pri\">buildable.commit</td><td></td>"),
        "{page}"
    );
    assert!(
        page.contains("<td class=\"hm-pri\">step.timestamp</td><td>1.5</td>"),
        "{page}"
    );
    assert!(
        page.contains("<td class=\"hm-pri\">target.phid</td><td>PHID-HMBT-1</td>"),
        "{page}"
    );
    assert!(page.contains("<td>pass</td><td>Consumed</td>"), "{page}");
    // The last 25 lines of the log, numbered from 5 like PHP; the empty
    // log is hidden.
    assert!(page.contains("Build Log 1 (remote - stdout)"), "{page}");
    assert!(page.contains("<tr><th>5</th><td>line6</td></tr>"), "{page}");
    assert!(
        page.contains("<tr><th>29</th><td>line30</td></tr>"),
        "{page}"
    );
    assert!(!page.contains("<td>line5</td>"), "{page}");
    assert!(page.contains("1 empty logs are hidden."), "{page}");
    assert!(
        page.contains("<a href=\"/harbormaster/build/1/2/?l=0\">Unlimited</a>"),
        "{page}"
    );
    assert!(page.contains("<strong>25</strong>"), "{page}");
    assert!(
        page.contains("<a href=\"/p/root/\">root</a> restarted this build."),
        "{page}"
    );
    assert!(!page.contains("/harbormaster/build/restart/1/"), "{page}");

    let page = page_as(test_db, "/harbormaster/build/1/2/?l=0", ALICE).await;
    assert!(page.contains("<tr><th>1</th><td>line1</td></tr>"), "{page}");
    let page = page_as(test_db, "/harbormaster/build/1/1/", ALICE).await;
    assert!(
        page.contains("You are viewing an older run of this build."),
        "{page}"
    );
    assert!(page.contains("Queued at 1970-01-01 00:00 UTC"), "{page}");
    assert!(page.contains("Waiting for 10 m"), "{page}");
    assert!(!page.contains("Treeherder"), "{page}");
    for run in ["0", "3"] {
        assert_eq!(
            status(test_db, &format!("/harbormaster/build/1/{run}/"), ALICE).await,
            StatusCode::NOT_FOUND
        );
    }
    let page = page_as(test_db, "/harbormaster/build/1/", ROOT).await;
    assert!(
        page.contains("<a href=\"/harbormaster/build/restart/1/\">"),
        "{page}"
    );

    // The secret plan: its step is "deleted", its build unnamed, and no one
    // may send it commands.
    let page = page_as(test_db, "/harbormaster/build/2/", ROOT).await;
    assert!(page.contains("Restricted Build Plan"), "{page}");
    assert!(
        page.contains("This build step has since been deleted"),
        "{page}"
    );
    assert!(!page.contains("/harbormaster/build/abort/2/"), "{page}");

    assert_eq!(
        status(test_db, "/harbormaster/build/3/", BOB).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fallback(test_db, "/harbormaster/build/1/?l=-1", Some(ALICE)).await,
        "a negative number of log lines"
    );
}

async fn plans(test_db: &TestDatabase) {
    let page = page_as(test_db, "/harbormaster/plan/1/", ALICE).await;
    assert!(page.contains("Try</h1>"), "{page}");
    assert!(
        page.contains("<a href=\"/harbormaster/step/view/1/\">Ping CI</a>"),
        "{page}"
    );
    assert!(
        page.contains("Make an HTTP <strong>GET</strong> request to <strong>ci.example</strong>."),
        "{page}"
    );
    assert!(
        page.contains("<a href=\"/harbormaster/step/view/2/\">Abort Older Builds</a>"),
        "{page}"
    );
    assert!(page.contains("Step 1.2"), "{page}");
    // Hold Drafts is "Never", the others their default.
    assert!(
        page.contains("<td class=\"hm-pri\">Hold Drafts</td>\n          <td>Never</td>"),
        "{page}"
    );
    assert!(
        page.contains("<td class=\"hm-pri\">Runnable</td>\n          <td>If Editable</td>"),
        "{page}"
    );
    // Alice can't edit the plan.
    assert!(!page.contains("/harbormaster/plan/edit/1/"), "{page}");
    assert!(!page.contains("/harbormaster/plan/run/1/"), "{page}");
    // The enabled rules; the personal one's author is disabled.
    assert!(page.contains("<a href=\"/H1\">Run Try</a>"), "{page}");
    assert!(page.contains("Author Not Active"), "{page}");
    assert!(!page.contains("/H3"), "{page}");
    assert!(page.contains("Affects: Differential Diffs"), "{page}");
    // Recent builds: build 2 runs another plan.
    assert!(
        page.contains("<a href=\"/harbormaster/build/4/\">Try</a>"),
        "{page}"
    );
    assert!(
        page.contains("Diff 1 · <a href=\"/p/alice/\">alice</a>"),
        "{page}"
    );
    assert!(!page.contains("/harbormaster/build/2/"), "{page}");
    // Bob can't see D2, so not build 3.
    let bob = page_as(test_db, "/harbormaster/plan/1/", BOB).await;
    assert!(!bob.contains("/harbormaster/build/3/"), "{bob}");
    assert!(page.contains("/harbormaster/build/3/"), "{page}");
    // The timeline hides what the plan was created with.
    assert!(
        page.contains("<a href=\"/p/root/\">root</a> created this object."),
        "{page}"
    );
    assert!(!page.contains("renamed this build plan"), "{page}");
    assert!(!page.contains("visibility"), "{page}");
    assert!(
        page.contains(
            "changed the <em>Hold Drafts</em> behavior for this plan from <em>Always</em> to \
             <em>Never</em>."
        ),
        "{page}"
    );

    let page = page_as(test_db, "/harbormaster/plan/1/", ROOT).await;
    assert!(
        page.contains("<a href=\"/harbormaster/plan/edit/1/\">"),
        "{page}"
    );
    assert!(
        page.contains("<a href=\"/harbormaster/plan/run/1/\">"),
        "{page}"
    );
    assert!(
        page.contains("<a class=\"hm-button\" href=\"/harbormaster/step/add/1/\">"),
        "{page}"
    );

    let page = page_as(test_db, "/harbormaster/plan/4/", ALICE).await;
    assert!(
        page.contains("does not have any build steps yet, so it will not do anything"),
        "{page}"
    );
    // Alice can edit this one: commands of the plan are links.
    assert!(
        page.contains("<a href=\"/harbormaster/plan/disable/4/\">"),
        "{page}"
    );

    assert_eq!(
        status(test_db, "/harbormaster/plan/2/", ROOT).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(test_db, "/harbormaster/plan/9/", ROOT).await,
        StatusCode::NOT_FOUND
    );
}

async fn lists(test_db: &TestDatabase) {
    let page = page_as(test_db, "/harbormaster/", ALICE).await;
    let b3 = page.find("<a href=\"/B3\">Diff 3</a>").expect("B3");
    let b2 = page.find("<a href=\"/B2\">D2: Hidden</a>").expect("B2");
    let b1 = page.find("<a href=\"/B1\">D1: Open</a>").expect("B1");
    assert!(b3 < b2 && b2 < b1, "{page}");
    assert!(page.contains("Manual</span>"), "{page}");
    assert!(
        page.contains("<p class=\"hm-attribute muted\">Diff 1</p>"),
        "{page}"
    );
    let page = page_as(test_db, "/harbormaster/", BOB).await;
    assert!(!page.contains("/B2"), "{page}");
    assert_eq!(
        fallback(test_db, "/harbormaster/?manual=1", Some(ALICE)).await,
        "a search query"
    );

    let page = page_as(test_db, "/harbormaster/plan/", ALICE).await;
    let empty = page.find(">Empty</a>").expect("Empty");
    let try_plan = page.find(">Try</a>").expect("Try");
    assert!(empty < try_plan, "{page}");
    assert!(!page.contains("Old"), "{page}");
    assert!(!page.contains("Secret"), "{page}");
    assert!(
        page.contains("<a class=\"tag\" href=\"/tag/ci/\">CI</a>"),
        "{page}"
    );
    assert!(page.contains("Restricted Project"), "{page}");
    assert!(page.contains("No Projects"), "{page}");
    // Bob saved his own query.
    assert_eq!(
        fallback(test_db, "/harbormaster/plan/", Some(BOB)).await,
        "saved search queries"
    );
}

/// The build and plan queries, build logs and build steps.
async fn more_pages(test_db: &TestDatabase) {
    // "My Builds": Alice started build 1.
    let page = page_as(test_db, "/harbormaster/build/", ALICE).await;
    assert!(page.contains("<h1>My Builds</h1>"), "{page}");
    assert!(
        page.contains("Build 1</span> <a href=\"/harbormaster/build/1/\">Try</a>"),
        "{page}"
    );
    assert!(!page.contains("/harbormaster/build/4/"), "{page}");
    let page = page_as(test_db, "/harbormaster/build/query/all/", ALICE).await;
    assert!(page.contains("/harbormaster/build/4/"), "{page}");
    assert!(
        page.contains("Build 2</span> <a href=\"/harbormaster/build/2/\">Build</a>"),
        "{page}"
    );
    let page = page_as(test_db, "/harbormaster/build/query/completed/", ALICE).await;
    assert!(page.contains("/harbormaster/build/1/"), "{page}");
    assert!(!page.contains("/harbormaster/build/4/"), "{page}");
    assert_eq!(
        fallback(test_db, "/harbormaster/build/query/abcdef/", Some(ALICE)).await,
        "a saved build query"
    );
    let page = page_as(test_db, "/harbormaster/plan/query/all/", ALICE).await;
    assert!(page.contains(">Old</a>"), "{page}");
    assert!(page.contains("<h1>All Plans</h1>"), "{page}");

    // The log of target 1, all of it, then its last lines.
    let page = page_as(test_db, "/harbormaster/log/view/1/", ALICE).await;
    assert!(page.contains("Build Log 1 (remote - stdout)"), "{page}");
    assert!(page.contains("<th>1</th><td>line1</td>"), "{page}");
    assert!(page.contains("<th>30</th><td>line30</td>"), "{page}");
    assert!(page.contains("href=\"/harbormaster/build/1/\""), "{page}");
    let page = page_as(test_db, "/harbormaster/log/view/1/?l=25", ALICE).await;
    assert!(!page.contains("<td>line1</td>"), "{page}");
    assert_eq!(
        status(test_db, "/harbormaster/log/view/99/", ALICE).await,
        StatusCode::NOT_FOUND
    );

    // The HTTP step of the Try plan, which only Root edits.
    let page = page_as(test_db, "/harbormaster/step/view/1/", ALICE).await;
    assert!(page.contains("Build Step 1: Ping CI</h1>"), "{page}");
    assert!(
        page.contains("<dt>Step Type</dt><dd>Make HTTP Request</dd>"),
        "{page}"
    );
    assert!(
        page.contains("<dt>URI</dt><dd>https://ci.example/run</dd>"),
        "{page}"
    );
    assert!(page.contains("<dt>HTTP Method</dt><dd>GET</dd>"), "{page}");
    assert!(page.contains("<strong>CI</strong>"), "{page}");
    assert!(
        !page.contains("href=\"/harbormaster/step/edit/1/\""),
        "{page}"
    );
    let page = page_as(test_db, "/harbormaster/step/view/1/", ROOT).await;
    assert!(
        page.contains("href=\"/harbormaster/step/edit/1/\""),
        "{page}"
    );
}

async fn revision(test_db: &TestDatabase) {
    let page = page_as(test_db, "/D1", ALICE).await;
    assert!(page.contains("<dt>Build Status</dt>"), "{page}");
    assert!(page.contains("<a href=\"/B1\">Buildable 1</a>"), "{page}");
    assert!(
        page.contains(
            "<a href=\"/harbormaster/build/1/\">Build 1: Try</a> <span \
             class=\"muted\">Passed</span> · <a href=\"https://treeherder.example/1\""
        ),
        "{page}"
    );
    assert!(page.contains("Build 2: Build</a>"), "{page}");
    assert!(!page.contains("These changes have"), "{page}");
    // The Lint field links the files of the diff; the Unit field shows
    // the diff's status; the failures show the "Unit Tests" box.
    assert!(page.contains("No Lint Coverage"), "{page}");
    assert!(page.contains("<a href=\"#C10NL3\">a.c:3</a>"), "{page}");
    assert!(page.contains("<td class=\"hm-pri\">b.c:7</td>"), "{page}");
    assert!(page.contains("No Test Coverage"), "{page}");
    assert!(page.contains("<h2>Unit Tests"), "{page}");
    assert!(page.contains("py &#62; Suite::test_fails"), "{page}");
    assert!(
        page.contains("This revision is currently a draft."),
        "{page}"
    );
    let page = page_as(test_db, "/D2", ALICE).await;
    let lint = page
        .find("These changes have lint problems.")
        .expect("lint");
    let build = page
        .find("These changes have failed to build.")
        .expect("build");
    let unit = page
        .find("Unit tests were skipped when generating these changes.")
        .expect("unit");
    assert!(lint < build && build < unit, "{page}");
    assert!(page.contains("Lint Errors"), "{page}");
    assert!(page.contains("Tests Skipped"), "{page}");
    // D2's buildable has no results: no "Unit Tests" box.
    assert!(!page.contains("<h2>Unit Tests"), "{page}");
    assert!(!page.contains("currently a draft"), "{page}");
    assert_eq!(
        fallback(test_db, "/D3", Some(ALICE)).await,
        "legacy lint messages"
    );
}

async fn unit_tests(test_db: &TestDatabase) {
    let page = page_as(test_db, "/B1", ALICE).await;
    assert!(
        page.contains("<h2>Unit Tests <span class=\"tag\">"),
        "{page}"
    );
    // Worst first, and five of the six.
    let fails = page.find("py &#62; Suite::test_fails").expect("fails");
    let breaks = page.find(">test_breaks<").expect("breaks");
    let last = page.find(">test_last<").expect("last");
    let passes = page.find(">test_passes<").expect("passes");
    assert!(fails < breaks && breaks < last && last < passes, "{page}");
    assert!(!page.contains("test_skips"), "{page}");
    assert_eq!(
        page.matches("href=\"/harbormaster/unit/view/").count(),
        5,
        "{page}"
    );
    assert!(
        page.contains("<td class=\"hm-right\">1,234 ms</td>"),
        "{page}"
    );
    // Three lines of text details, escaped; Remarkup rendered.
    assert!(page.contains("one\ntwo &lt;b&gt;\nthree\n</div>"), "{page}");
    assert!(!page.contains("four"), "{page}");
    assert!(page.contains("<strong>loud</strong>"), "{page}");
    assert!(
        page.contains("View Full Test Results (1 Failed · 1 Broken · 3 Passed · 1 Skipped)"),
        "{page}"
    );
    assert!(page.contains("href=\"/harbormaster/unit/1/\""), "{page}");
}

async fn checks(test_db: &TestDatabase) {
    // File artifacts show the file, or that it is restricted.
    let page = page_as(test_db, "/harbormaster/build/1/", ALICE).await;
    assert!(
        page.contains("<td>File</td><td><a href=\"/F1\">F1: build.log</a></td>"),
        "{page}"
    );
    assert!(page.contains("Restricted File"), "{page}");

    // Steps run in rounds; one depends on a missing step and never runs.
    let page = page_as(test_db, "/harbormaster/plan/5/", ALICE).await;
    let first = page
        .find("Step 1.1</span> <a href=\"/harbormaster/step/view/10/\">")
        .expect("1");
    let second = page
        .find("Step 2.1</span> <a href=\"/harbormaster/step/view/11/\">")
        .expect("2");
    let stuck = page
        .find("Step 3.1</span> <a href=\"/harbormaster/step/view/12/\">")
        .expect("3");
    assert!(first < second && second < stuck, "{page}");
    assert!(page.contains("Build Step <strong>First</strong>"), "{page}");
    assert!(
        page.contains("This dependency specifies a build step which doesn"),
        "{page}"
    );
    assert!(
        page.contains("This build plan will deadlock when executed"),
        "{page}"
    );
}

#[tokio::test]
async fn harbormaster_pages_show_what_the_viewer_may_see() {
    let Some(test_db) = setup().await else {
        return;
    };
    buildables(&test_db).await;
    unit_tests(&test_db).await;
    checks(&test_db).await;
    builds(&test_db).await;
    plans(&test_db).await;
    lists(&test_db).await;
    more_pages(&test_db).await;
    revision(&test_db).await;

    // Buildables of other objects are PHP's, in the list too.
    test_db
        .execute(
            "INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
               containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) \
             VALUES (4, 'PHID-HMBB-4', 'PHID-TEST-1', NULL, 'passed', 40, 41, 0)",
        )
        .await;
    assert_eq!(
        fallback(&test_db, "/B4", Some(ALICE)).await,
        "buildables of other objects"
    );
    assert_eq!(
        fallback(&test_db, "/harbormaster/", Some(ALICE)).await,
        "buildables of other objects"
    );
    test_db.drop_databases().await;
}
