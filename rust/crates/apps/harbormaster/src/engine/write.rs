//! The rows the engine writes, in the order PHP saves them, and their
//! writing in one MySQL transaction.
//!
//! Lisk's `save()` writes every column back; the engine updates only those
//! it changed (and `dateModified`), having checked that the others, the
//! JSON ones in particular, would be written back as they are stored.

use phab_app_differential::edit;
use phab_conduit::{Context, MethodError};
use phab_db::Database;
use phab_db::sqlx::MySqlConnection;
use phab_db::worker::{self, Task};

use super::EngineError;
use super::log::Log;
use super::rows::TargetRow;

/// The data of a queued task.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TaskData {
    /// Encoded already.
    Json(String),
    /// `{"targetID": <id>}` of the `n`-th new target.
    Target(usize),
}

/// One write.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Op {
    /// `HarbormasterBuild::save()`.
    Build {
        id: i64,
        status: String,
        generation: i64,
    },
    /// A `harbormaster_buildtransaction` without a comment.
    Transaction {
        phid: String,
        author: String,
        object: String,
        kind: String,
        new_value: String,
        metadata: String,
        content_source: &'static str,
    },
    /// A message marked consumed.
    Message { id: i64 },
    /// `HarbormasterBuildTarget::save()` of an existing target.
    Target {
        id: i64,
        status: String,
        started: Option<i64>,
        completed: Option<i64>,
    },
    /// A new target.
    NewTarget {
        name: Option<String>,
        target: TargetRow,
    },
    /// An artifact released.
    Artifact { id: i64 },
    /// `HarbormasterBuildable::save()`.
    Buildable {
        id: i64,
        status: String,
        container: Option<String>,
    },
    /// `PhabricatorWorker::scheduleTask()`.
    Task {
        class: &'static str,
        data: TaskData,
        priority: i64,
        object_phid: Option<String>,
    },
    /// `DifferentialBuildableEngine::publishBuildable()` changing the
    /// revision's status of the buildable: the revision's editor.
    Publish {
        revision_id: i64,
        buildable_phid: String,
        status: Option<&'static str>,
    },
    /// A log written and closed.
    Log {
        phid: String,
        target_phid: String,
        source: String,
        kind: String,
        log: Log,
    },
}

/// How the revision editor's refusal ends the task.
fn edit_error(error: MethodError) -> EngineError {
    match error {
        MethodError::Unsupported(what) => EngineError::Unsupported(what.to_owned()),
        other => EngineError::Failed(format!("{other:?}")),
    }
}

/// Plans the revision edits of `ops` ([`Op::Publish`]) without writing:
/// what Differential's editor leaves to PHP is [`EngineError::Unsupported`].
pub(super) async fn check(context: &Context, ops: &[Op], now: i64) -> Result<(), EngineError> {
    for op in ops {
        if let Op::Publish {
            revision_id,
            buildable_phid,
            status,
        } = op
        {
            edit::publish_buildable(
                context,
                *revision_id,
                (buildable_phid, *status),
                (now, false),
            )
            .await
            .map_err(edit_error)?;
        }
    }
    Ok(())
}

/// Writes `ops` at `now`: in one transaction, but for the revision edits,
/// which are the editor's own (once [`check()`] passes).
pub(super) async fn apply(context: &Context, ops: &[Op], now: i64) -> Result<(), EngineError> {
    check(context, ops, now).await?;
    let database = &context.database;
    let mut transaction = database.pool().begin().await?;
    let mut new_targets = Vec::new();
    for op in ops {
        if let Op::Publish {
            revision_id,
            buildable_phid,
            status,
        } = op
        {
            transaction.commit().await?;
            edit::publish_buildable(
                context,
                *revision_id,
                (buildable_phid, *status),
                (now, true),
            )
            .await
            .map_err(edit_error)?;
            transaction = database.pool().begin().await?;
            continue;
        }
        apply_one(&mut transaction, database, op, now, &mut new_targets).await?;
    }
    transaction.commit().await?;
    Ok(())
}

fn table(database: &Database, name: &str) -> String {
    database.table("harbormaster", name)
}

// One arm per row kind, each its query.
#[allow(clippy::too_many_lines)]
async fn apply_one(
    connection: &mut MySqlConnection,
    database: &Database,
    op: &Op,
    now: i64,
    new_targets: &mut Vec<u64>,
) -> Result<(), EngineError> {
    match op {
        Op::Build {
            id,
            status,
            generation,
        } => {
            phab_db::sqlx::query(&format!(
                "UPDATE {} SET buildStatus = ?, buildGeneration = ?, dateModified = ? WHERE id = ?",
                table(database, "harbormaster_build")
            ))
            .bind(status)
            .bind(generation)
            .bind(now)
            .bind(id)
            .execute(&mut *connection)
            .await?;
        }
        Op::Transaction {
            phid,
            author,
            object,
            kind,
            new_value,
            metadata,
            content_source,
        } => {
            // `populateTransaction()` for the omnipotent actor: public,
            // editable by no one.
            phab_db::sqlx::query(&format!(
                "INSERT INTO {} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, \
                 commentPHID, commentVersion, transactionType, oldValue, newValue, \
                 contentSource, metadata, dateCreated, dateModified) VALUES (?, ?, ?, 'public', \
                 'no-one', NULL, 0, ?, 'null', ?, ?, ?, ?, ?)",
                table(database, "harbormaster_buildtransaction")
            ))
            .bind(phid)
            .bind(author)
            .bind(object)
            .bind(kind)
            .bind(new_value)
            .bind(content_source)
            .bind(metadata)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
        }
        Op::Message { id } => {
            phab_db::sqlx::query(&format!(
                "UPDATE {} SET isConsumed = 1, dateModified = ? WHERE id = ?",
                table(database, "harbormaster_buildmessage")
            ))
            .bind(now)
            .bind(id)
            .execute(&mut *connection)
            .await?;
        }
        Op::Target {
            id,
            status,
            started,
            completed,
        } => {
            phab_db::sqlx::query(&format!(
                "UPDATE {} SET targetStatus = ?, dateStarted = ?, dateCompleted = ?, \
                 dateModified = ? WHERE id = ?",
                table(database, "harbormaster_buildtarget")
            ))
            .bind(status)
            .bind(started)
            .bind(completed)
            .bind(now)
            .bind(id)
            .execute(&mut *connection)
            .await?;
        }
        Op::NewTarget { name, target } => {
            new_targets
                .push(insert_target(connection, database, name.as_deref(), target, now).await?);
        }
        Op::Artifact { id } => {
            phab_db::sqlx::query(&format!(
                "UPDATE {} SET isReleased = 1, dateModified = ? WHERE id = ?",
                table(database, "harbormaster_buildartifact")
            ))
            .bind(now)
            .bind(id)
            .execute(&mut *connection)
            .await?;
        }
        Op::Buildable {
            id,
            status,
            container,
        } => {
            phab_db::sqlx::query(&format!(
                "UPDATE {} SET buildableStatus = ?, containerPHID = ?, dateModified = ? WHERE \
                 id = ?",
                table(database, "harbormaster_buildable")
            ))
            .bind(status)
            .bind(container)
            .bind(now)
            .bind(id)
            .execute(&mut *connection)
            .await?;
        }
        Op::Task {
            class,
            data,
            priority,
            object_phid,
        } => {
            let data = match data {
                TaskData::Json(data) => data.clone(),
                TaskData::Target(position) => {
                    let id = new_targets.get(*position).ok_or_else(|| {
                        EngineError::Failed("a target task without its target".to_owned())
                    })?;
                    format!("{{\"targetID\":{id}}}")
                }
            };
            worker::schedule_task(
                connection,
                database,
                Task {
                    class,
                    data: &data,
                    priority: *priority,
                    object_phid: object_phid.as_deref(),
                },
                now,
            )
            .await?;
        }
        // The editor's, in `apply()`.
        Op::Publish { .. } => {}
        Op::Log {
            phid,
            target_phid,
            source,
            kind,
            log,
        } => {
            insert_log(
                connection,
                database,
                (phid, target_phid),
                (source, kind),
                log,
                now,
            )
            .await?;
        }
    }
    Ok(())
}

/// Inserts a new target; returns its ID.
async fn insert_target(
    connection: &mut MySqlConnection,
    database: &Database,
    name: Option<&str>,
    target: &TargetRow,
    now: i64,
) -> Result<u64, EngineError> {
    Ok(phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, name, buildPHID, buildStepPHID, className, details, variables, \
         targetStatus, dateStarted, dateCompleted, buildGeneration, dateCreated, dateModified) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?)",
        table(database, "harbormaster_buildtarget")
    ))
    .bind(&target.phid)
    .bind(name)
    .bind(&target.build_phid)
    .bind(&target.step_phid)
    .bind(&target.class_name)
    .bind(&target.details)
    .bind(&target.variables)
    .bind(&target.status)
    .bind(target.generation)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?
    .last_insert_id())
}

/// Inserts a closed log and its chunks: `openBuildLog()` saves the new
/// log, the flushes save it with each chunk, `closeBuildLog()` with its
/// duration.
async fn insert_log(
    connection: &mut MySqlConnection,
    database: &Database,
    (phid, target_phid): (&str, &str),
    (source, kind): (&str, &str),
    log: &Log,
    now: i64,
) -> Result<(), EngineError> {
    let log_id = phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, buildTargetPHID, logSource, logType, duration, live, filePHID, \
         byteLength, chunkFormat, lineMap, dateCreated, dateModified) VALUES (?, ?, ?, ?, 0, 0, \
         NULL, ?, 'text', ?, ?, ?)",
        table(database, "harbormaster_buildlog")
    ))
    .bind(phid)
    .bind(target_phid)
    .bind(source)
    .bind(kind)
    .bind(u64::try_from(log.byte_length).unwrap_or(u64::MAX))
    .bind(&log.line_map)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    for (head, data) in &log.chunks {
        let head = u64::try_from(*head).unwrap_or(u64::MAX);
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (logID, encoding, headOffset, tailOffset, size, chunk) VALUES (?, \
             'text', ?, ?, ?, ?)",
            table(database, "harbormaster_buildlogchunk")
        ))
        .bind(log_id)
        .bind(head)
        .bind(head + size)
        .bind(size)
        .bind(data.as_slice())
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}
