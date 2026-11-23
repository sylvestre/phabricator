//! `PhabricatorMetaMTAWorker` against a throwaway `MySQL` database: the
//! messages the test mailer gets and the `metamta_mail` rows left, compared
//! to what PHP's `PhabricatorMetaMTAMail::sendWithMailers()` and
//! `PhabricatorMailEmailEngine::newMessage()` produce for the same rows
//! (derived from the PHP code, see each test), and mail left to PHP.
//!
//! The task data is what `PhabricatorMetaMTAMail::save()` queues: the mail
//! ID. Parameters are stored as PHP's `json_encode()` writes them.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use md5::{Digest as _, Md5};
use phab_app_metamta::send::{Outbox, Recorded};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, rows};
use phab_worker::handlers::metamta::MetaMTAWorker;
use phab_worker::{Executed, LeaseOwner, Registry, Worker};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const CLASS: &str = "PhabricatorMetaMTAWorker";
const DATABASES: &[&str] = &["worker", "metamta", "user", "project"];
const BASE_URI: &str = "http://phab.example.com/";

struct Fixture {
    test_db: TestDatabase,
}

/// Users: alice sends; bob and erin receive; carol is disabled; dave
/// muted the `differential-other` tag. Bob wants plain text, `Re:` and
/// static subjects. Each has the settings cache PHP builds on login.
async fn fixture() -> Option<Fixture> {
    let test_db = TestDatabase::create(DATABASES).await?;
    let users = [
        ("alice", "Alice Liddell", 0),
        ("bob", "Bob Builder", 0),
        ("carol", "Carol Danvers", 1),
        ("dave", "Dave Lister", 0),
        ("erin", "", 0),
    ];
    let index = phab_core::hash::digest_for_index(b"user.preferences.v2");
    for (name, real_name, disabled) in users {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, \
                   dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
                   isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
                   isMailingList) VALUES \
                 ('PHID-USER-{name}', '{name}', '{real_name}', 1, 1, 'c', 0, {disabled}, 0, 1, \
                   1, 's', 0, 0); \
                 INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, isPrimary, \
                   dateCreated, dateModified) VALUES \
                 ('PHID-EADR-{name}', 'PHID-USER-{name}', '{name}@example.com', 1, 1, 1, 1); \
                 INSERT INTO `{{ns}}_user`.user_cache (userPHID, cacheIndex, cacheKey, \
                   cacheData, cacheType) VALUES \
                 ('PHID-USER-{name}', '{index}', 'user.preferences.v2', '[]', 'preferences')"
            ))
            .await;
    }
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_preferences (userPHID, preferences, dateCreated, \
               dateModified, phid, builtinKey) VALUES \
             ('PHID-USER-dave', '{\"mailtags\":{\"differential-other\":0}}', 1, 1, \
               'PHID-PSET-dave', NULL), \
             ('PHID-USER-bob', '{\"html-emails\":\"text\",\"re-prefix\":\"re\",\
               \"vary-subject\":\"static\"}', 1, 1, 'PHID-PSET-bob', NULL)",
        )
        .await;
    Some(Fixture { test_db })
}

/// The configuration: the base URI, `extra` and one test mailer unless
/// `extra` sets `cluster.mailers`.
fn config(extra: &Value) -> Config {
    let mut layer = Map::new();
    layer.insert("phabricator.base-uri".to_owned(), json!(BASE_URI));
    layer.insert(
        "cluster.mailers".to_owned(),
        json!([{"key": "test-mailer", "type": "test"}]),
    );
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        layer.insert(key, value);
    }
    let mut config = Config::default();
    config.push_layer("test", layer);
    config
}

/// The parameters `PhabricatorMetaMTAMail` stores for a Differential
/// update from alice: those `$parameters` holds once the transaction
/// editor set everything, in the order it sets them.
fn parameters(to: &[&str], cc: &[&str]) -> Value {
    json!({
        "sensitive": false,
        "mustEncrypt": false,
        "subject": "D7: Fix the \u{201c}thing\u{201d}",
        "from": "PHID-USER-alice",
        "subject-prefix": "[Differential]",
        "vary-subject-prefix": "[Updated]",
        "exclude": [],
        "muted": [],
        "herald-force-recipients": [],
        "mailtags": ["differential-other"],
        "is-bulk": true,
        "body": "alice updated this revision.\n\nREVISION DETAIL\n  http://phab.example.com/D7\n",
        "html-body": "<div>alice updated this revision.</div>",
        "headers": [
            ["X-Herald-Rules", "<1>"],
            ["X-Phabricator-To", "<PHID-USER-bob>"],
        ],
        "stamps": {"2": "application(Differential)", "0": "actor(@alice)"},
        "to": to,
        "cc": cc,
    })
}

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    /// A queued mail; returns its ID.
    async fn mail(&self, id: i64, parameters: &Value) -> i64 {
        let sql = format!(
            "INSERT INTO {} (id, phid, actorPHID, parameters, status, message, relatedPHID, \
               dateCreated, dateModified) VALUES \
             (?, ?, 'PHID-USER-alice', ?, 'queued', NULL, 'PHID-DREV-seven', 1, 1)",
            self.database().table("metamta", "metamta_mail")
        );
        sqlx::query(&sql)
            .bind(id)
            .bind(format!("PHID-MTAM-{id}"))
            .bind(phab_core::php::json_encode(parameters))
            .execute(self.database().pool())
            .await
            .unwrap();
        id
    }

    async fn schedule(&self, data: &Value) -> i64 {
        let data = phab_core::php::json_encode(data);
        let mut connection = self.database().pool().acquire().await.unwrap();
        let id = schedule_task(
            &mut connection,
            self.database(),
            Task {
                class: CLASS,
                data: &data,
                priority: 1000,
                object_phid: None,
            },
            1_600_000_000,
        )
        .await
        .unwrap();
        i64::try_from(id).unwrap()
    }

    fn worker(&self, config: Config, outbox: &Outbox) -> Worker {
        let mut registry = Registry::default();
        registry.register(MetaMTAWorker::with_outbox(outbox.clone()));
        Worker::new(
            self.database().clone(),
            config,
            registry,
            LeaseOwner::new("rust-test"),
        )
    }

    /// Queues the worker for mail `id` and runs it.
    async fn run(&self, config: Config, id: i64) -> (Executed, Vec<Recorded>) {
        let outbox: Outbox = Arc::new(Mutex::new(Vec::new()));
        let task = self.schedule(&json!(id)).await;
        let worker = self.worker(config, &outbox);
        let ran = worker.work_on(None).await.unwrap().expect("a task to run");
        assert_eq!(ran.id, task);
        let sent = outbox.lock().unwrap().clone();
        (ran.outcome.unwrap(), sent)
    }

    /// The row of mail `id`.
    async fn row(&self, id: i64) -> MailRow {
        let sql = format!(
            "SELECT CAST(parameters AS CHAR) parameters, CAST(status AS CHAR) status, \
               CAST(message AS CHAR) message, CAST(actorPHID AS CHAR) actorPHID, \
               CAST(relatedPHID AS CHAR) relatedPHID, dateModified FROM {} WHERE id = ?",
            self.database().table("metamta", "metamta_mail")
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_one(self.database().pool())
            .await
            .unwrap();
        MailRow {
            parameters: rows::text(&row, "parameters").unwrap(),
            status: rows::text(&row, "status").unwrap(),
            message: rows::opt_text(&row, "message").unwrap(),
            actor_phid: rows::opt_text(&row, "actorPHID").unwrap(),
            related_phid: rows::opt_text(&row, "relatedPHID").unwrap(),
            date_modified: rows::int(&row, "dateModified").unwrap(),
        }
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

#[derive(Debug)]
struct MailRow {
    parameters: String,
    status: String,
    message: Option<String>,
    actor_phid: Option<String>,
    related_phid: Option<String>,
    date_modified: i64,
}

/// `$parameters` after `sendWithMailers()` sent the mail: the engine's
/// `headers.unfiltered` and `headers.sent` (always empty, see
/// `flattenHeaders()`), then the mailer key and the deliverability of
/// every actor (`actors.sent`), the routing rules and their map.
fn sent_parameters(mut parameters: Value, actors: &Value) -> String {
    let map = parameters.as_object_mut().unwrap();
    map.insert("headers.unfiltered".to_owned(), json!([]));
    map.insert("headers.sent".to_owned(), json!([]));
    map.insert("mailer.key".to_owned(), json!("test-mailer"));
    map.insert("actors.sent".to_owned(), actors.clone());
    map.insert("routing.sent".to_owned(), Value::Null);
    map.insert("routingmap.sent".to_owned(), json!([]));
    phab_core::php::json_encode(&parameters)
}

/// The guts of a sent message, `X-Phabricator-Send-Attempt` (16 random
/// characters of `[a-z2-7]`) checked and replaced by `<attempt>`.
fn guts(sent: &Recorded) -> Value {
    let mut guts = sent.guts();
    for header in guts["headers"].as_array_mut().unwrap() {
        if header[0] == "X-Phabricator-Send-Attempt" {
            let attempt = header[1].as_str().unwrap();
            assert_eq!(attempt.len(), 16);
            assert!(
                attempt
                    .bytes()
                    .all(|byte| matches!(byte, b'a'..=b'z' | b'2'..=b'7'))
            );
            header[1] = json!("<attempt>");
        }
    }
    guts
}

/// The headers of `newEmailHeaders()` for the mail of [`parameters`].
fn headers(mail_id: i64, threading: &[(&str, String)]) -> Value {
    let mut headers = vec![
        json!(["X-Phabricator-Sent-This-Message", "Yes"]),
        json!(["X-Mail-Transport-Agent", "MetaMTA"]),
        json!(["X-Auto-Response-Suppress", "All"]),
        json!(["X-Phabricator-Mail-Tags", "<differential-other>"]),
        json!(["X-Herald-Rules", "<1>"]),
        json!(["X-Phabricator-To", "<PHID-USER-bob>"]),
        json!(["Precedence", "bulk"]),
        json!(["Thread-Topic", "PHID-DREV-seven"]),
        json!(["X-Phabricator-Mail-ID", mail_id.to_string()]),
        json!(["X-Phabricator-Send-Attempt", "<attempt>"]),
    ];
    for (name, value) in threading {
        headers.push(json!([name, value]));
    }
    headers.push(json!([
        "X-Phabricator-Stamps",
        "application(Differential) actor(@alice)"
    ]));
    Value::Array(headers)
}

/// A user actor, as `actors.sent` records it.
fn actor(deliverable: bool, reasons: &[&str]) -> Value {
    json!({"deliverable": deliverable, "reasons": reasons})
}

/// A plain update to bob: sent as one message by the test mailer.
///
/// `newFromEmailAddress()` keeps the default address (`noreply@` the
/// base URI's domain, `metamta.can-send-as-user` is off) and takes the
/// sender's full name; the subject has the vary prefix (global
/// preferences, `metamta.one-mail-per-recipient` off) and the HTML body
/// is sent (global default `html`).
#[tokio::test]
async fn sends_a_mail_to_one_user() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let parameters = parameters(&["PHID-USER-erin"], &[]);
    let id = fixture.mail(11, &parameters).await;
    let config = config(&json!({"metamta.one-mail-per-recipient": false}));
    let (outcome, sent) = fixture.run(config, id).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");

    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].mailer, "test-mailer");
    assert_eq!(
        guts(&sent[0]),
        json!({
            "from": "\"alice (Alice Liddell)\" <noreply@phab.example.com>",
            "tos": ["erin@example.com"],
            "ccs": [],
            "subject": "[Differential] [Updated] D7: Fix the \u{201c}thing\u{201d}",
            "headers": headers(id, &[]),
            "body": "alice updated this revision.\n\nREVISION DETAIL\n  http://phab.example.com/D7\n",
            "html-body": "<div>alice updated this revision.</div>",
            "attachments": [],
            "did-send": true,
        })
    );

    let row = fixture.row(id).await;
    assert_eq!(row.status, "sent");
    assert_eq!(row.message, None);
    assert_eq!(row.actor_phid.as_deref(), Some("PHID-USER-alice"));
    assert_eq!(row.related_phid.as_deref(), Some("PHID-DREV-seven"));
    assert!(row.date_modified > 1);
    assert_eq!(
        row.parameters,
        sent_parameters(
            parameters,
            &json!({
                "PHID-USER-alice": actor(true, &[]),
                "PHID-USER-erin": actor(true, &[]),
            })
        )
    );
    fixture.finish().await;
}

/// Dave muted the mail's only tag (`mailtags` setting `0`): he is left out
/// with reason `mailtags`; alone, he leaves no recipient and the mail is
/// voided with the engine's message, its parameters written back as they
/// were.
#[tokio::test]
async fn leaves_out_a_recipient_who_muted_the_tag() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let config = || config(&json!({}));

    let alone = parameters(&["PHID-USER-dave"], &[]);
    let id = fixture.mail(21, &alone).await;
    let (outcome, sent) = fixture.run(config(), id).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");
    assert_eq!(sent, Vec::<Recorded>::new());
    let row = fixture.row(id).await;
    assert_eq!(row.status, "void");
    assert_eq!(
        row.message.as_deref(),
        Some(
            "Message has no valid recipients: all To/CC are disabled, invalid, or configured \
             not to receive this mail."
        )
    );
    assert_eq!(row.parameters, phab_core::php::json_encode(&alone));

    let with_erin = parameters(&["PHID-USER-dave", "PHID-USER-erin"], &[]);
    let id = fixture.mail(22, &with_erin).await;
    let (_, sent) = fixture.run(config(), id).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(guts(&sent[0])["tos"], json!(["erin@example.com"]));
    let row = fixture.row(id).await;
    assert_eq!(row.status, "sent");
    assert_eq!(
        row.parameters,
        sent_parameters(
            with_erin,
            &json!({
                "PHID-USER-alice": actor(true, &[]),
                "PHID-USER-dave": actor(false, &["mailtags"]),
                "PHID-USER-erin": actor(true, &[]),
            })
        )
    );
    fixture.finish().await;
}

/// Carol is disabled (`REASON_DISABLED` from the actor query): with only
/// her in "To", the engine puts the default address there and erin stays
/// in "Cc". The disabled sender of another mail is still the sender.
#[tokio::test]
async fn skips_a_disabled_recipient() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let parameters = parameters(&["PHID-USER-carol"], &["PHID-USER-erin"]);
    let id = fixture.mail(31, &parameters).await;
    let (outcome, sent) = fixture.run(config(&json!({})), id).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");
    assert_eq!(sent.len(), 1);
    let guts = guts(&sent[0]);
    assert_eq!(
        guts["tos"],
        json!(["\"Phabricator\" <noreply@phab.example.com>"])
    );
    assert_eq!(guts["ccs"], json!(["erin@example.com"]));
    let row = fixture.row(id).await;
    assert_eq!(
        row.parameters,
        sent_parameters(
            parameters,
            &json!({
                "PHID-USER-alice": actor(true, &[]),
                "PHID-USER-carol": actor(false, &["disabled"]),
                "PHID-USER-erin": actor(true, &[]),
            })
        )
    );

    let alone = self::parameters(&["PHID-USER-carol"], &[]);
    let id = fixture.mail(32, &alone).await;
    let (_, sent) = fixture.run(config(&json!({})), id).await;
    assert_eq!(sent, Vec::<Recorded>::new());
    let row = fixture.row(id).await;
    assert_eq!(row.status, "void");
    assert_eq!(row.parameters, phab_core::php::json_encode(&alone));
    fixture.finish().await;
}

/// A reply in a thread: the test adapter doesn't support `Message-ID`
/// (its property is `null`), so the thread ID, wrapped as
/// `<id@domain>`, goes in `In-Reply-To` and `References`, followed by
/// the parent message, and `Thread-Index` is the base64 of the first 27
/// hex digits of its MD5, a space and the time packed big-endian.
#[tokio::test]
async fn threads_a_reply() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let mut parameters = parameters(&["PHID-USER-erin"], &[]);
    let map = parameters.as_object_mut().unwrap();
    map.insert(
        "thread-id".to_owned(),
        json!("differential-rev-PHID-DREV-seven-req"),
    );
    map.insert("is-first-message".to_owned(), json!(false));
    map.insert(
        "parent-message-id".to_owned(),
        json!("<parent@mail.example>"),
    );
    let id = fixture.mail(41, &parameters).await;
    let (_, sent) = fixture
        .run(
            config(&json!({"metamta.one-mail-per-recipient": false})),
            id,
        )
        .await;
    assert_eq!(sent.len(), 1);
    let row = fixture.row(id).await;
    assert_eq!(row.status, "sent");

    let thread = "<differential-rev-PHID-DREV-seven-req@phab.example.com>";
    let mut index = hex::encode(Md5::digest(thread.as_bytes())).as_bytes()[..27].to_vec();
    index.push(b' ');
    index.extend_from_slice(&u32::try_from(row.date_modified).unwrap().to_be_bytes());
    let index = base64::engine::general_purpose::STANDARD.encode(index);
    let guts = guts(&sent[0]);
    assert_eq!(
        guts["headers"],
        headers(
            id,
            &[
                ("In-Reply-To", "<parent@mail.example>".to_owned()),
                ("References", format!("{thread} <parent@mail.example>")),
                ("Thread-Index", index),
            ]
        )
    );
    // Threaded, but the global preferences have no `Re:`.
    assert_eq!(
        guts["subject"],
        "[Differential] [Updated] D7: Fix the \u{201c}thing\u{201d}"
    );
    assert_eq!(
        row.parameters,
        sent_parameters(
            parameters,
            &json!({
                "PHID-USER-alice": actor(true, &[]),
                "PHID-USER-erin": actor(true, &[]),
            })
        )
    );
    fixture.finish().await;
}

/// `metamta.one-mail-per-recipient` decides whose preferences format the
/// message (`loadPreferences()`): bob's when on (`Re:`, static subject,
/// plain text), the global ones when off.
#[tokio::test]
async fn formats_with_the_recipients_preferences_when_mailing_each() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let mut parameters = parameters(&["PHID-USER-bob"], &[]);
    parameters
        .as_object_mut()
        .unwrap()
        .insert("thread-id".to_owned(), json!("t"));

    let id = fixture.mail(51, &parameters).await;
    let (_, sent) = fixture
        .run(config(&json!({"metamta.one-mail-per-recipient": true})), id)
        .await;
    let each = guts(&sent[0]);
    assert_eq!(
        each["subject"],
        "Re: [Differential] D7: Fix the \u{201c}thing\u{201d}"
    );
    assert_eq!(each.get("html-body"), None);

    let id = fixture.mail(52, &parameters).await;
    let (_, sent) = fixture
        .run(
            config(&json!({"metamta.one-mail-per-recipient": false})),
            id,
        )
        .await;
    let shared = guts(&sent[0]);
    assert_eq!(
        shared["subject"],
        "[Differential] [Updated] D7: Fix the \u{201c}thing\u{201d}"
    );
    assert_eq!(
        shared["html-body"],
        "<div>alice updated this revision.</div>"
    );

    for id in [51, 52] {
        let row = fixture.row(id).await;
        assert_eq!(row.status, "sent");
        assert_eq!(
            row.parameters,
            sent_parameters(
                parameters.clone(),
                &json!({
                    "PHID-USER-alice": actor(true, &[]),
                    "PHID-USER-bob": actor(true, &[]),
                })
            )
        );
    }
    fixture.finish().await;
}

/// Without mailers (the development stack), `sendWithMailers()` voids the
/// mail before loading anyone; with only an SES mailer, mail nobody
/// receives is voided too, but mail to deliver is left to PHP untouched.
#[tokio::test]
async fn voids_without_mailers_and_leaves_ses_mail_to_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let parameters = parameters(&["PHID-USER-erin"], &[]);
    let id = fixture.mail(61, &parameters).await;
    let (_, sent) = fixture
        .run(config(&json!({"cluster.mailers": []})), id)
        .await;
    assert_eq!(sent, Vec::<Recorded>::new());
    let row = fixture.row(id).await;
    assert_eq!(row.status, "void");
    assert_eq!(row.message.as_deref(), Some("No mailers are configured."));
    assert_eq!(row.parameters, phab_core::php::json_encode(&parameters));

    let ses = || {
        config(
            &json!({"cluster.mailers": [{"key": "ses", "type": "ses", "options": {
            "access-key": "a", "secret-key": "s", "region": "r", "endpoint": "e"}}]}),
        )
    };
    let nobody = self::parameters(&["PHID-USER-dave"], &[]);
    let id = fixture.mail(62, &nobody).await;
    fixture.run(ses(), id).await;
    assert_eq!(fixture.row(id).await.status, "void");

    let id = fixture.mail(63, &parameters).await;
    let task = fixture.schedule(&json!(id)).await;
    let outbox: Outbox = Arc::new(Mutex::new(Vec::new()));
    let worker = fixture.worker(ses(), &outbox);
    assert!(worker.work_on(None).await.unwrap().is_none());
    assert!(worker.declined.reason(task).is_some());
    let row = fixture.row(id).await;
    assert_eq!(row.status, "queued");
    assert_eq!(row.date_modified, 1);
    fixture.finish().await;
}

/// A mail which does not load fails permanently; one already sent is left
/// alone.
#[tokio::test]
async fn fails_without_the_mail_and_skips_sent_mail() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let (outcome, _) = fixture.run(config(&json!({})), 404).await;
    let Executed::FailedPermanently { message, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        message,
        "Unable to load mail message (with ID \"404\") while preparing to deliver it."
    );

    let id = fixture
        .mail(71, &parameters(&["PHID-USER-erin"], &[]))
        .await;
    fixture
        .test_db
        .execute("UPDATE `{ns}_metamta`.metamta_mail SET status = 'sent' WHERE id = 71")
        .await;
    let (outcome, sent) = fixture.run(config(&json!({})), id).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");
    assert_eq!(sent, Vec::<Recorded>::new());
    assert_eq!(fixture.row(id).await.date_modified, 1);
    fixture.finish().await;
}

/// What a fake SMTP server received.
#[derive(Debug, Default)]
struct Session {
    commands: Vec<String>,
    data: Vec<u8>,
}

/// A one-session SMTP server accepting everything.
async fn smtp_server() -> (u16, tokio::task::JoinHandle<Session>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut session = Session::default();
        write.write_all(b"220 test ESMTP\r\n").await.unwrap();
        loop {
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let command = line.trim_end().to_owned();
            session.commands.push(command.clone());
            if command.starts_with("EHLO") {
                write
                    .write_all(b"250-test\r\n250 8BITMIME\r\n")
                    .await
                    .unwrap();
            } else if command == "DATA" {
                write.write_all(b"354 go\r\n").await.unwrap();
                let mut data = Vec::new();
                while !data.ends_with(b"\r\n.\r\n") {
                    let mut byte = [0u8; 1];
                    read.read_exact(&mut byte).await.unwrap();
                    data.push(byte[0]);
                }
                session.data = data;
                write.write_all(b"250 queued\r\n").await.unwrap();
            } else {
                write.write_all(b"250 ok\r\n").await.unwrap();
            }
        }
        session
    });
    (port, handle)
}

/// Through SMTP, the bytes are those `PHPMailer` builds: headers in its
/// order, the custom headers `EncodeHeader()`d (the subject in Q
/// encoding), the plain text body (bob wants text) in base64, lines in
/// CRLF and the data ended by an empty line and the dot.
#[tokio::test]
async fn sends_the_bytes_phpmailer_sends() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let parameters = parameters(&["PHID-USER-bob"], &[]);
    let id = fixture.mail(81, &parameters).await;
    let (port, server) = smtp_server().await;
    let config = config(&json!({
        "metamta.one-mail-per-recipient": true,
        "cluster.mailers": [{"key": "smtp", "type": "smtp", "options": {
            "host": "127.0.0.1", "port": port}}],
    }));
    let (outcome, sent) = fixture.run(config, id).await;
    assert!(matches!(outcome, Executed::Succeeded(_)), "{outcome:?}");
    assert_eq!(sent, Vec::<Recorded>::new());
    let session = server.await.unwrap();
    assert_eq!(
        session.commands,
        [
            "EHLO localhost.localdomain",
            "MAIL FROM:<noreply@phab.example.com>",
            "RCPT TO:<bob@example.com>",
            "DATA",
        ]
    );
    let row = fixture.row(id).await;
    assert_eq!(row.status, "sent");
    let mut expected_parameters: Value = serde_json::from_str(&sent_parameters(
        parameters,
        &json!({
            "PHID-USER-alice": actor(true, &[]),
            "PHID-USER-bob": actor(true, &[]),
        }),
    ))
    .unwrap();
    expected_parameters["mailer.key"] = json!("smtp");
    assert_eq!(
        row.parameters,
        phab_core::php::json_encode(&expected_parameters)
    );

    let received = String::from_utf8(session.data).unwrap();
    let attempt = received
        .split("X-Phabricator-Send-Attempt: ")
        .nth(1)
        .unwrap()
        .get(..16)
        .unwrap()
        .to_owned();
    let date = jiff::Timestamp::from_second(row.date_modified)
        .unwrap()
        .strftime("%a, %-d %b %Y %H:%M:%S +0000")
        .to_string();
    let body = base64::engine::general_purpose::STANDARD
        .encode("alice updated this revision.\n\nREVISION DETAIL\n  http://phab.example.com/D7\n");
    let expected = format!(
        "Date: {date}\r\n\
         Return-Path: noreply@phab.example.com\r\n\
         To: bob@example.com\r\n\
         From: \"alice (Alice Liddell)\" <noreply@phab.example.com>\r\n\
         Subject: =?utf-8?Q?[Differential]_D7:_Fix_the_=E2=80=9Cthing=E2=80=9D?=\r\n\
         X-Priority: 3\r\n\
         X-Mailer: PHPMailer 5.1 (phpmailer.sourceforge.net)\r\n\
         X-Phabricator-Sent-This-Message: Yes\r\n\
         X-Mail-Transport-Agent: MetaMTA\r\n\
         X-Auto-Response-Suppress: All\r\n\
         X-Phabricator-Mail-Tags: <differential-other>\r\n\
         X-Herald-Rules: <1>\r\n\
         X-Phabricator-To: <PHID-USER-bob>\r\n\
         Precedence: bulk\r\n\
         Thread-Topic: PHID-DREV-seven\r\n\
         X-Phabricator-Mail-ID: {id}\r\n\
         X-Phabricator-Send-Attempt: {attempt}\r\n\
         X-Phabricator-Stamps: application(Differential) actor(@alice)\r\n\
         MIME-Version: 1.0\r\n\
         Content-Transfer-Encoding: base64\r\n\
         Content-Type: text/plain; charset=\"utf-8\"\r\n\
         \r\n\
         {}\r\n\
         \r\n\
         \r\n\
         .\r\n",
        body.as_bytes()
            .chunks(76)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join("\r\n")
    );
    assert_eq!(received, expected);
    fixture.finish().await;
}
