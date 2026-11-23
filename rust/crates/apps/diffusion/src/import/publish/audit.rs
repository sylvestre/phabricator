//! `applyTransactions()` of the publish worker: `PhabricatorAuditEditor`
//! applying the commit's `audit:commit` transaction as the omnipotent
//! user, acting as the committer (or the author, or Diffusion), from the
//! daemons.
//!
//! The transaction is saved, the commit's audit status recomputed (it
//! stays "none": commits with audits are left to PHP) and the
//! `IMPORTED_PUBLISH` flag written. Herald runs with the commit adapter
//! (`HeraldCommitAdapter`): its transcript is saved, with no rule (enabled
//! commit rules are left to PHP, as their fields are not ported), and so is
//! the X-Herald-Rules header. Then the transaction publish task, with the
//! editor's state (recipients, mail stamps, the patch and the affected
//! files), which PHP's `PhabricatorApplicationTransactionPublishWorker`
//! runs for commits.
//!
//! Left to PHP: commits with audits or an audit status, commit messages
//! naming auditors, mentioning users, referencing objects (but the
//! revision the commit closes, and objects which are not mentionable),
//! embedding files, reverting or depending on objects; commits with
//! projects or Harbormaster buildables, enabled Herald rules on commits,
//! and commits which are not mostly imported (whose mentions, mail and
//! feed PHP turns off).

use phab_app_herald::engine::{self, Subject};
use phab_conduit::Context;
use phab_core::{Phid, PhidType, php, random};
use phab_db::{Database, edges, rows, worker};
use serde_json::{Value, json};

use crate::import::commit::{CommitData, ParserTask};
use crate::import::publish::close::{self, CommitRef};
use crate::import::text::summarize_commit_message;
use crate::import::{IMPORTED_CHANGE, IMPORTED_MESSAGE, ImportError};

/// `PhabricatorDaemonContentSource`.
const DAEMON_SOURCE: &str = r#"{"source":"daemon","params":[]}"#;
/// The PHID of `PhabricatorDiffusionApplication`.
const DIFFUSION_APPLICATION: &str = "PHID-APPS-PhabricatorDiffusionApplication";
/// The publish task the editor queues.
const PUBLISH_WORKER: &str = "PhabricatorApplicationTransactionPublishWorker";
/// `PhabricatorAuditEditor::MAX_FILES_SHOWN_IN_EMAIL`.
const MAX_FILES_SHOWN_IN_EMAIL: usize = 1000;
/// `DifferentialChangeType::TYPE_CHILD`.
const TYPE_CHILD: i64 = 10;
/// `DifferentialChangeType::FILE_DIRECTORY`.
pub(super) const FILE_DIRECTORY: i64 = 2;

/// The prefixes of `DifferentialCustomFieldDependsOnParser` and
/// `DifferentialCustomFieldRevertsParser`: a message with any of them is
/// left to PHP, which may find objects after them.
const RELATION_WORDS: &[&str] = &[
    "depends",
    "revert",
    "backout",
    "backsout",
    "backedout",
    "back out",
    "backs out",
    "backed out",
    "undo",
];

/// The object references which mention nothing: projects, Herald rules,
/// Spaces and repositories are not `PhabricatorMentionableInterface`.
const UNMENTIONABLE_PREFIXES: &[&str] = &["#", "H", "S", "r", "R"];

/// The writes of the editor.
#[derive(Debug)]
pub struct Edit {
    xaction_phid: String,
    actor: String,
    new_value: String,
    metadata: String,
    epoch: i64,
    transcript: engine::TranscriptRow,
    herald_header: String,
    hostname: String,
    commit_phid: String,
    /// The publish task's data, without the patch's state.
    worker_data: Value,
}

/// A path the commit changed (`DiffusionPathChange`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    /// `getPath()`: without the leading `/`.
    pub path: String,
    /// `getFileType()`.
    pub file_type: i64,
}

/// `DiffusionPathChangeQuery::loadChanges()`: the direct changes of the
/// commit, but those to children, ordered by path.
pub async fn path_changes(
    database: &Database,
    commit_id: i64,
) -> Result<Vec<PathChange>, ImportError> {
    let sql = format!(
        "SELECT c.changeType, c.fileType, p.path pathName FROM {} c LEFT JOIN {} p ON \
         c.pathID = p.id WHERE c.commitID = ? AND isDirect = 1",
        database.table("repository", "repository_pathchange"),
        database.table("repository", "repository_path"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(commit_id)
        .fetch_all(database.pool())
        .await?;
    let mut changes: Vec<(String, i64, i64)> = Vec::new();
    for row in &found {
        let Some(path) = rows::opt_text(row, "pathName")? else {
            return Err(ImportError::unsupported("path changes without their path"));
        };
        changes.push((
            path,
            rows::int(row, "changeType")?,
            rows::int(row, "fileType")?,
        ));
    }
    // isort(): stable, and paths, which start with "/", compare as strings.
    changes.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(changes
        .into_iter()
        .filter(|(_, change_type, _)| *change_type != TYPE_CHILD)
        .map(|(path, _, file_type)| PathChange {
            path: path.trim_start_matches('/').to_owned(),
            file_type,
        })
        .collect())
}

/// `didApplyHeraldRules()`: the affected files of the mail
/// (`PhabricatorOwnerPathQuery::loadAffectedPaths()`, sorted and
/// truncated).
fn affected_files(changes: &[PathChange]) -> String {
    let mut files: Vec<String> = changes
        .iter()
        .map(|change| {
            let path = format!("/{}", change.path);
            if change.file_type == FILE_DIRECTORY {
                format!("{}/", path.trim_end_matches('/'))
            } else {
                path
            }
        })
        .collect();
    files.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    if files.len() > MAX_FILES_SHOWN_IN_EMAIL {
        files.truncate(MAX_FILES_SHOWN_IN_EMAIL);
        files.push(format!(
            "(This commit affected more than {MAX_FILES_SHOWN_IN_EMAIL} files. Only \
             {MAX_FILES_SHOWN_IN_EMAIL} are shown here and additional ones are truncated.)"
        ));
    }
    files.join("\n")
}

/// `getIdentityDisplayPHID()` of an identity, if it exists.
async fn identity_display_phid(
    database: &Database,
    phid: Option<&str>,
) -> Result<Option<String>, ImportError> {
    let Some(phid) = phid else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT phid, currentEffectiveUserPHID FROM {} WHERE phid = ?",
        database.table("repository", "repository_identity")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
    else {
        return Ok(None);
    };
    let user = rows::opt_text(&row, "currentEffectiveUserPHID")?.filter(|user| !user.is_empty());
    Ok(Some(match user {
        Some(user) => user,
        None => rows::text(&row, "phid")?,
    }))
}

/// `getPublishAsPHID()`: the committer, the author, or Diffusion.
async fn acting_phid(database: &Database, task: &ParserTask) -> Result<String, ImportError> {
    let commit = &task.commit;
    if let Some(phid) =
        identity_display_phid(database, commit.committer_identity_phid.as_deref()).await?
    {
        return Ok(phid);
    }
    if let Some(phid) =
        identity_display_phid(database, commit.author_identity_phid.as_deref()).await?
    {
        return Ok(phid);
    }
    Ok(DIFFUSION_APPLICATION.to_owned())
}

/// The destinations of the commit's edges of a type, newest first.
async fn destinations(
    database: &Database,
    phid: &str,
    edge_type: i64,
) -> Result<Vec<String>, ImportError> {
    Ok(
        edges::load_edges(database, &[Phid::new(phid)], &[edge_type], None)
            .await?
            .into_iter()
            .map(|edge| edge.dst.as_str().to_owned())
            .collect(),
    )
}

/// What the editor can't do as PHP does, with the commit as it is.
async fn check_commit(database: &Database, task: &ParserTask) -> Result<(), ImportError> {
    let commit = &task.commit;
    // isCommitMostlyImported(): otherwise no mention, mail or feed story.
    if !commit.is_partially_imported(IMPORTED_MESSAGE | IMPORTED_CHANGE) {
        return Err(ImportError::unsupported(
            "publishing commits which are not mostly imported",
        ));
    }
    // updateAuditStatus() of a commit without audits keeps it at "none".
    let sql = format!(
        "SELECT 1 FROM {} WHERE commitPHID = ? LIMIT 1",
        database.table("repository", "repository_auditrequest")
    );
    let audited = phab_db::sqlx::query(&sql)
        .bind(&commit.phid)
        .fetch_optional(database.pool())
        .await?
        .is_some();
    if audited || commit.audit_status != "none" {
        return Err(ImportError::unsupported("publishing commits with audits"));
    }
    if !destinations(database, &commit.phid, edges::OBJECT_HAS_PROJECT)
        .await?
        .is_empty()
    {
        return Err(ImportError::unsupported("publishing commits with projects"));
    }
    // HarbormasterBuildable::applyBuildPlans() and the message to the
    // commit's buildables.
    let sql = format!(
        "SELECT 1 FROM {} WHERE buildablePHID = ? LIMIT 1",
        database.table("harbormaster", "harbormaster_buildable")
    );
    if phab_db::sqlx::query(&sql)
        .bind(&commit.phid)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(ImportError::unsupported(
            "publishing commits with buildables",
        ));
    }
    match engine::load_rules(database, "commit").await {
        Ok(rules) if rules.is_empty() => Ok(()),
        Ok(_) => Err(ImportError::unsupported("Herald rules on commits")),
        Err(engine::HeraldError::Unsupported(reason)) => Err(ImportError::unsupported(reason)),
        Err(engine::HeraldError::Database(error)) => Err(ImportError::Database(error)),
        Err(engine::HeraldError::Query(error)) => Err(error.into()),
    }
}

/// `expandTransaction()` and `expandSupportTransactions()` of the commit
/// transaction must add nothing: no auditor from the message
/// (`getAuditRequestTransactionPHIDsFromCommitMessage()`), no mention, no
/// file, no revert or dependency. `unmentionable` is the revision the
/// commit closes.
async fn check_message(
    context: &Context,
    message: &str,
    unmentionable: Option<i64>,
    now: i64,
) -> Result<(), ImportError> {
    let parsed = close::parse_message(context, message, now).await?;
    let auditors = parsed["fields"].get("phabricator:auditors");
    if auditors.is_some_and(php::is_truthy) {
        return Err(ImportError::unsupported(
            "commit messages which name auditors",
        ));
    }
    let lower = message.to_ascii_lowercase();
    if RELATION_WORDS.iter().any(|word| lower.contains(word)) {
        return Err(ImportError::unsupported(
            "commit messages which may revert or depend on objects",
        ));
    }
    let engine = phab_remarkup::phabricator::engine(phab_app_differential::markup::engine_config(
        &context.config,
    ));
    let prepared = engine.prepare(message.as_bytes(), phab_remarkup::Mode::Html);
    if !prepared.mentioned_usernames().is_empty() {
        return Err(ImportError::unsupported("commit messages mentioning users"));
    }
    if !prepared.file_attach_intent_ids().is_empty() {
        return Err(ImportError::unsupported("commit messages naming files"));
    }
    let closed = unmentionable.map(|id| id.to_string().into_bytes());
    for (prefix, id) in prepared.object_references() {
        let allowed = UNMENTIONABLE_PREFIXES.contains(&prefix)
            || (prefix == "D" && closed.as_ref() == Some(&id));
        if !allowed {
            return Err(ImportError::unsupported(
                "commit messages mentioning objects",
            ));
        }
    }
    Ok(())
}

/// `getXHeraldRulesHeader()` as the `herald` mail stamp: `<N>` becomes
/// `HN`.
fn rule_monograms(header: &str) -> Value {
    Value::Array(
        header
            .split([',', ' '])
            .filter(|part| !part.is_empty())
            .map(|part| part.trim_matches(['<', '>']))
            .filter(|item| !item.is_empty() && item.bytes().all(|byte| byte.is_ascii_digit()))
            .map(|item| json!(format!("H{item}")))
            .collect(),
    )
}

/// `loadXHeraldRulesHeader()`.
async fn saved_header(database: &Database, phid: &str) -> Result<Option<String>, ImportError> {
    let sql = format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::text(&row, "header"))
        .transpose()?)
}

/// The recipients of the publish task.
struct Recipients {
    to: Vec<String>,
    subscribers: Vec<String>,
    muted: Vec<String>,
}

/// The `state` of the publish task (`getWorkerState()`), but the custom
/// state, which [`Edit::write`] completes with the patch.
fn worker_state(
    task: &ParserTask,
    actor: &str,
    recipients: &Recipients,
    herald_header: &str,
    affected_files: &str,
) -> Value {
    let commit = &task.commit;
    let mut notify: Vec<&str> = Vec::new();
    for phid in recipients.to.iter().chain(&recipients.subscribers) {
        if !notify.contains(&phid.as_str()) {
            notify.push(phid);
        }
    }
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    let stamps = vec![
        stamp("string", "application", json!("Diffusion")),
        stamp(
            "string",
            "monogram",
            json!(commit.monogram(&task.repository)),
        ),
        stamp("string", "phid", json!(commit.phid)),
        stamp("string", "object-type", json!("CMIT")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!("daemon")),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!([])),
        stamp("bool", "new", json!(false)),
        stamp("phid", "mention", json!([])),
        stamp("string", "herald", rule_monograms(herald_header)),
        stamp("phid", "removed", json!([])),
        stamp("phid", "tag", json!([])),
        stamp("phid", "subscriber", json!(recipients.subscribers)),
    ];
    json!({
        "parentMessageID": null,
        "isNewObject": false,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": herald_header,
        "mailToPHIDs": recipients.to,
        "mailCCPHIDs": recipients.subscribers,
        "feedNotifyPHIDs": notify,
        "feedRelatedPHIDs": [commit.phid, actor],
        "feedShouldPublish": true,
        "mailShouldSend": true,
        "mustEncrypt": [],
        "mailStamps": stamps,
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": recipients.muted,
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": {
            "rawPatch": null,
            "affectedFiles": affected_files,
            "auditorPHIDs": [],
        },
        "custom.encoding": {"rawPatch": "binary"},
    })
}

/// `newPublishTransactions()`: the new value of the commit transaction.
fn new_value(task: &ParserTask, data: &CommitData) -> Result<Value, ImportError> {
    let commit_ref = CommitRef::of(data)?;
    Ok(json!({
        "description": data.commit_message,
        "summary": summarize_commit_message(&data.commit_message),
        "authorName": close::author_string(data, &commit_ref),
        "authorPHID": task.commit.author_phid,
        "committerName": close::committer_string(data, &commit_ref),
        "committerPHID": data.detail("committerPHID").cloned().unwrap_or(Value::Null),
    }))
}

/// Plans the editor's writes for the commit, whose data has the details
/// `closeRevisions()` set; `raw_patch` is the patch the mail gets.
pub async fn plan(
    context: &Context,
    task: &ParserTask,
    data: &CommitData,
    now: i64,
) -> Result<Edit, ImportError> {
    let database = &context.database;
    let commit = &task.commit;
    check_commit(database, task).await?;
    // loadRevisionForCommit(): the revision isn't mentioned again.
    let revision_id = match data.detail("differential.revisionID") {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    }
    .filter(|id| *id != 0);
    let unmentionable = match revision_id {
        Some(id) => close::revision_phid(database, id).await?.map(|_| id),
        None => None,
    };
    check_message(context, &data.commit_message, unmentionable, now).await?;

    let actor = acting_phid(database, task).await?;
    let xaction_phid = Phid::generate(PhidType::TRANSACTION, Some(PhidType::COMMIT)).into_string();
    let group_id = random::random_characters(32);
    let metadata = php::json_encode(&json!({"core.groupID": group_id}));
    let new_value = php::json_encode(&new_value(task, data)?);

    // HeraldEngine with no rule: the transcript of the commit alone.
    let subject = Subject {
        phid: commit.phid.clone(),
        content_type: "commit".to_owned(),
        name: commit.monogram(&task.repository),
        applied_transaction_phids: Some(vec![xaction_phid.clone()]),
    };
    let evaluation = engine::evaluate(&[], &subject, &[]);
    let transcript = evaluation.transcript(&subject, &[]);
    let herald_header = engine::combine_headers(
        saved_header(database, &commit.phid).await?.as_deref(),
        &engine::rules_header(&[]),
    );
    let affected = affected_files(&path_changes(database, commit.id).await?);

    // getMailTo(): the author, the auditors (none) and the actor;
    // getMailCC(): the subscribers.
    let mut to: Vec<String> = commit
        .author_phid
        .iter()
        .filter(|phid| !phid.is_empty())
        .cloned()
        .collect();
    to.push(actor.clone());
    let recipients = Recipients {
        to,
        subscribers: destinations(database, &commit.phid, edges::OBJECT_HAS_SUBSCRIBER).await?,
        muted: destinations(database, &commit.phid, edges::MUTED_BY).await?,
    };
    let state = worker_state(task, &actor, &recipients, &herald_header, &affected);
    let worker_data = json!({
        "objectPHID": commit.phid,
        "actorPHID": actor,
        "xactionPHIDs": [xaction_phid],
        "state": state,
    });
    Ok(Edit {
        xaction_phid,
        actor,
        new_value,
        metadata,
        epoch: commit.epoch,
        transcript,
        herald_header,
        hostname: context.hostname.clone(),
        commit_phid: commit.phid.clone(),
        worker_data,
    })
}

impl Edit {
    /// Sets the patch of the publish task's custom state:
    /// `STORAGE_ENCODING_BINARY` is `base64_encode(serialize())`.
    pub fn set_raw_patch(&mut self, raw_patch: &[u8]) {
        use base64::Engine as _;
        let serialized =
            phab_db::phpser::serialize(&phab_db::phpser::Value::Str(raw_patch.to_vec()));
        self.worker_data["state"]["custom"]["rawPatch"] =
            json!(base64::engine::general_purpose::STANDARD.encode(serialized));
    }

    /// Writes the transaction and the commit (`applyTransactions()`), the
    /// transcript and the header (`applyHeraldRules()`), and queues the
    /// publish task (`queuePublishing()`).
    pub async fn write(
        &self,
        database: &Database,
        task: &mut ParserTask,
        now: i64,
    ) -> Result<(), ImportError> {
        let mut transaction = database.pool().begin().await?;
        // The commit's own save() writes it as it was loaded.
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, \
             commentPHID, commentVersion, transactionType, oldValue, newValue, contentSource, \
             metadata, dateCreated, dateModified) VALUES (?, ?, ?, 'public', 'no-one', NULL, 0, \
             'audit:commit', 'null', ?, ?, ?, ?, ?)",
            database.table("audit", "audit_transaction")
        ))
        .bind(&self.xaction_phid)
        .bind(&self.actor)
        .bind(&self.commit_phid)
        .bind(&self.new_value)
        .bind(DAEMON_SOURCE)
        .bind(&self.metadata)
        .bind(self.epoch)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        // applyFinalEffects().
        task.commit
            .write_import_status_flag(database, crate::import::IMPORTED_PUBLISH)
            .await?;

        let mut connection = database.pool().acquire().await?;
        let transcript = &self.transcript;
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, time, host, duration, objectPHID, dryRun, objectTranscript, \
             ruleTranscripts, conditionTranscripts, applyTranscripts, garbageCollected) VALUES \
             (?, ?, ?, ?, ?, 0, ?, ?, ?, ?, 0)",
            database.table("herald", "herald_transcript")
        ))
        .bind(Phid::generate(PhidType::HERALD_TRANSCRIPT, None).as_str())
        .bind(now)
        .bind(&self.hostname)
        .bind(transcript.duration)
        .bind(&transcript.object_phid)
        .bind(&transcript.object_transcript)
        .bind(&transcript.rule_transcripts)
        .bind(&transcript.condition_transcripts)
        .bind(&transcript.apply_transcripts)
        .execute(&mut *connection)
        .await?;
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, header) VALUES (?, ?) ON DUPLICATE KEY UPDATE header = \
             VALUES(header)",
            database.table("herald", "herald_savedheader")
        ))
        .bind(&self.commit_phid)
        .bind(&self.herald_header)
        .execute(&mut *connection)
        .await?;
        let data = php::json_encode(&self.worker_data);
        worker::schedule_task(
            &mut connection,
            database,
            worker::Task {
                class: PUBLISH_WORKER,
                data: &data,
                priority: worker::PRIORITY_ALERTS,
                object_phid: Some(&self.commit_phid),
            },
            now,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_affected_files_like_php() {
        let change = |path: &str, file_type| PathChange {
            path: path.to_owned(),
            file_type,
        };
        assert_eq!(
            affected_files(&[
                change("dir/a.txt", 1),
                change("README", 1),
                change("dir", FILE_DIRECTORY),
            ]),
            "/README\n/dir/\n/dir/a.txt"
        );
        assert_eq!(affected_files(&[]), "");
    }

    #[test]
    fn names_herald_rules_like_php() {
        assert_eq!(rule_monograms("none"), json!([]));
        assert_eq!(rule_monograms("<3>, <12>"), json!(["H3", "H12"]));
    }
}
