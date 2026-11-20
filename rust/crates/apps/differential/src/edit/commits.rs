//! The commits a revision's remarkup references, which the mention edge
//! goes to: their inverse edit is `PhabricatorAuditEditor`'s, whose
//! transactions are in `audit_transaction` and whose publish task carries
//! the audit state.
//!
//! Ported for Git commits without audits which publish
//! (`shouldPublishRepositoryActivity()`); the others are left to PHP.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Phid, php};
use phab_db::{Database, edges, rows};
use serde_json::{Value, json};

use crate::commit;

/// `PhabricatorRepositoryCommit::IMPORTED_MESSAGE | IMPORTED_CHANGE`
/// (`isCommitMostlyImported()`).
const MOSTLY_IMPORTED: i64 = 1 | 2;
/// `PhabricatorRepositoryCommit::IMPORTED_PERMANENT`.
const IMPORTED_PERMANENT: i64 = 1024;

/// A commit an edge goes to.
pub(super) struct Linked {
    pub(super) phid: String,
    id: i64,
    author_phid: Option<String>,
    /// `getMonogram()`: the full hash with the repository.
    monogram: String,
    projects: Vec<String>,
    subscribers: Vec<String>,
    muted: Vec<String>,
}

/// The commits named by `identifiers` the viewer can see, in the order of
/// `DiffusionCommitQuery` (newest ID first).
pub(super) async fn load(
    request: &ApiRequest<'_>,
    identifiers: &[String],
) -> Result<Vec<Linked>, MethodError> {
    if identifiers.is_empty() {
        return Ok(Vec::new());
    }
    let context = &request.context;
    let database = &context.database;
    let found = commit::load_visible_by_identifiers(
        database,
        &context.config,
        &request.viewer,
        identifiers,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    let mut linked: Vec<Linked> = Vec::new();
    for commit in found.into_values() {
        if linked
            .iter()
            .any(|known| known.phid == commit.phid.as_str())
        {
            continue;
        }
        linked.push(plan(database, &commit).await?);
    }
    linked.sort_by_key(|commit| std::cmp::Reverse(commit.id));
    Ok(linked)
}

/// The commit's facts, when its inverse edit is one Rust ports.
async fn plan(database: &Database, commit: &commit::Commit) -> Result<Linked, MethodError> {
    let repository = &commit.repository;
    if repository.vcs != "git" {
        return Err(MethodError::Unsupported("commits of other repositories"));
    }
    let sql = format!(
        "SELECT id, authorPHID, auditStatus, importStatus FROM {} WHERE phid = ?",
        database.table("repository", "repository_commit")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(commit.phid.as_str())
        .fetch_one(database.pool())
        .await?;
    let import_status = rows::int(&row, "importStatus")?;
    // getCommitHoldReasons() and isCommitMostlyImported().
    let detail = |key| repository.detail(key).is_some_and(php::is_truthy);
    let mask = MOSTLY_IMPORTED | IMPORTED_PERMANENT;
    if detail("importing") || detail("herald-disabled") || import_status & mask != mask {
        return Err(MethodError::Unsupported("commits which don't publish"));
    }
    // updateAuditStatus() would change the status of a commit with audits.
    let sql = format!(
        "SELECT 1 FROM {} WHERE commitPHID = ? LIMIT 1",
        database.table("repository", "repository_auditrequest")
    );
    let audited = phab_db::sqlx::query(&sql)
        .bind(commit.phid.as_str())
        .fetch_optional(database.pool())
        .await?
        .is_some();
    if audited || rows::text(&row, "auditStatus")? != "none" {
        return Err(MethodError::Unsupported("commits with audits"));
    }
    let phid = Phid::new(commit.phid.as_str());
    let projects = super::destinations(database, &phid, edges::OBJECT_HAS_PROJECT).await?;
    super::check_projects(database, &projects).await?;
    let monogram = match &repository.callsign {
        Some(callsign) => format!("r{callsign}{}", commit.identifier),
        None => format!("R{}:{}", repository.id, commit.identifier),
    };
    Ok(Linked {
        phid: commit.phid.as_str().to_owned(),
        id: rows::int(&row, "id")?,
        author_phid: rows::opt_text(&row, "authorPHID")?,
        monogram,
        projects,
        subscribers: super::destinations(database, &phid, edges::OBJECT_HAS_SUBSCRIBER).await?,
        muted: super::destinations(database, &phid, edges::MUTED_BY).await?,
    })
}

/// The `state` of the commit's publish task (`getWorkerState()` of
/// `PhabricatorAuditEditor`), with the parent editor's `mustEncrypt`.
pub(super) fn worker_state(
    commit: &Linked,
    actor: &str,
    must_encrypt: &[String],
    herald_header: &str,
) -> Value {
    // getMailTo(): the author, the auditors and the actor.
    let mut to: Vec<&str> = commit.author_phid.iter().map(String::as_str).collect();
    to.push(actor);
    let mut notify: Vec<&str> = Vec::new();
    for phid in to
        .iter()
        .copied()
        .chain(commit.subscribers.iter().map(String::as_str))
    {
        if !notify.contains(&phid) {
            notify.push(phid);
        }
    }
    let mut related = vec![commit.phid.as_str(), actor];
    related.extend(commit.projects.iter().map(String::as_str));
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    let stamps = vec![
        stamp("string", "application", json!("Diffusion")),
        stamp("string", "monogram", json!(commit.monogram)),
        stamp("string", "phid", json!(commit.phid)),
        stamp("string", "object-type", json!("CMIT")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!("conduit")),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!(must_encrypt)),
        stamp("bool", "new", json!(false)),
        stamp("phid", "mention", json!([])),
        stamp("string", "herald", super::rule_monograms(herald_header)),
        stamp("phid", "removed", json!([])),
        stamp("phid", "tag", json!(commit.projects)),
        stamp("phid", "subscriber", json!(commit.subscribers)),
    ];
    json!({
        "parentMessageID": null,
        "isNewObject": false,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": herald_header,
        "mailToPHIDs": to,
        "mailCCPHIDs": commit.subscribers,
        "feedNotifyPHIDs": notify,
        "feedRelatedPHIDs": related,
        "feedShouldPublish": true,
        "mailShouldSend": true,
        "mustEncrypt": must_encrypt,
        "mailStamps": stamps,
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": commit.muted,
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        // The sub-editor has no patch (serialize(null), base64-encoded as
        // binary) and runs no Herald, which lists the affected files.
        "custom": {
            "rawPatch": "Tjs=",
            "affectedFiles": null,
            "auditorPHIDs": [],
        },
        "custom.encoding": {"rawPatch": "binary"},
    })
}
