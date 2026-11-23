//! `worker execute --id N`: `PhabricatorWorkerManagementExecuteWorkflow`
//! for the task classes `phab-worker` runs. Like PHP, it ignores the lease
//! (it clears it, then runs the task); unlike PHP, a task whose class
//! isn't ported, or which its handler declines, is left to `bin/worker`.

use phab_db::{Database, rows};
use phab_worker::{Executed, LeaseOwner, Worker, handlers};

use crate::Context;

const PHP_COMMAND: &str = "bin/worker execute";

/// Where a task is.
enum Found {
    Active(String),
    Archived(String),
    Missing,
}

async fn find(database: &Database, id: i64) -> Result<Found, phab_db::DbError> {
    for (table, archived) in [("worker_activetask", false), ("worker_archivetask", true)] {
        let sql = format!(
            "SELECT taskClass FROM {} WHERE id = ?",
            database.table("worker", table)
        );
        if let Some(row) = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
        {
            let class = rows::text(&row, "taskClass")?;
            return Ok(if archived {
                Found::Archived(class)
            } else {
                Found::Active(class)
            });
        }
    }
    Ok(Found::Missing)
}

/// `PhabricatorManagementWorkflow::logInfo()` and its siblings, on stderr.
fn log(context: &mut Context<'_>, color: u8, label: &str, message: &str) {
    if context.ansi {
        let _ = writeln!(
            context.err,
            "\x1b[{color}m\x1b[1m {label} \x1b[m\x1b[49m {message}"
        );
    } else {
        let _ = writeln!(context.err, " {label}  {message}");
    }
}

/// ANSI backgrounds of `logInfo()`, `logWarn()` and `logOkay()`.
const BLUE: u8 = 44;
const YELLOW: u8 = 43;
const GREEN: u8 = 42;

/// Runs `worker <workflow>`.
pub async fn run(context: &mut Context<'_>, arguments: &[String]) -> u8 {
    let Some((workflow, rest)) = arguments.split_first() else {
        return context.usage_error("Usage: worker execute --id <task id>");
    };
    if workflow != "execute" {
        return context.left_to_php(&format!("worker {workflow}"), "bin/worker");
    }
    let mut ids = Vec::new();
    let mut rest = rest.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--id" => match rest.next().map(|id| id.parse::<i64>()) {
                Some(Ok(id)) => ids.push(id),
                _ => return context.usage_error("--id needs a task ID."),
            },
            // Selecting by class, object or failure count, and running
            // archived tasks again, are PHP's.
            other if other.starts_with("--") => {
                return context.left_to_php(&format!("worker execute {other}"), PHP_COMMAND);
            }
            other => return context.usage_error(&format!("Unexpected argument \"{other}\".")),
        }
    }
    if ids.is_empty() {
        return context.left_to_php("worker execute without --id", PHP_COMMAND);
    }
    let (config, database) = match phab_worker::connect(&context.config_dir).await {
        Ok(connected) => connected,
        Err(error) => return context.error(&error),
    };
    execute(context, database, config, &ids).await
}

/// Runs the tasks `ids`, checking all of them before writing anything.
pub async fn execute(
    context: &mut Context<'_>,
    database: Database,
    config: phab_core::Config,
    ids: &[i64],
) -> u8 {
    let registry = handlers::registry();
    let mut runnable = Vec::new();
    for &id in ids {
        match find(&database, id).await {
            Ok(Found::Active(class)) if registry.get(&class).is_some() => {
                runnable.push((id, class));
            }
            Ok(Found::Active(class)) => {
                return context.left_to_php(
                    &format!("Task {id} ({class}): phab-worker doesn't run {class}"),
                    PHP_COMMAND,
                );
            }
            Ok(Found::Archived(class)) => log(
                context,
                YELLOW,
                "ARCHIVED",
                &format!(
                    "Task {id} ({class}) is already archived, and will not be executed. Use \
                     \"--retry\" to execute archived tasks."
                ),
            ),
            Ok(Found::Missing) => {
                return context.usage_error(&format!("No task exists with ID \"{id}\"."));
            }
            Err(error) => return context.error(&error.to_string()),
        }
    }
    if runnable.is_empty() {
        log(context, YELLOW, "NO TASKS", "No tasks selected to execute.");
        return 0;
    }
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "phab-cli".to_owned());
    let worker = Worker::new(database, config, registry, LeaseOwner::new(&host));
    let mut executed = 0;
    for (id, class) in runnable {
        // "This ignores leases": the lease goes, then the task runs.
        let sql = format!(
            "UPDATE {} SET leaseOwner = NULL, leaseExpires = ? WHERE id = ?",
            worker.database.table("worker", "worker_activetask")
        );
        if let Err(error) = phab_db::sqlx::query(&sql)
            .bind(context.now)
            .bind(id)
            .execute(worker.database.pool())
            .await
        {
            return context.error(&error.to_string());
        }
        log(
            context,
            BLUE,
            "EXECUTE",
            &format!("Executing Task {id} ({class})..."),
        );
        let ran = match worker.work_on(Some(&[id])).await {
            Ok(ran) => ran,
            Err(error) => return context.error(&error.to_string()),
        };
        let Some(ran) = ran else {
            let reason = worker
                .declined
                .reason(id)
                .unwrap_or_else(|| "the task could not be leased".to_owned());
            return context.left_to_php(&format!("Task {id} ({class}): {reason}"), PHP_COMMAND);
        };
        match ran.outcome {
            Ok(Executed::Declined { reason, .. }) => {
                return context.left_to_php(&format!("Task {id} ({class}): {reason}"), PHP_COMMAND);
            }
            Ok(Executed::Failed { message, .. } | Executed::FailedPermanently { message, .. }) => {
                return context.error(&format!("Task {id} ({class}) failed: {message}"));
            }
            Ok(Executed::Succeeded(_) | Executed::Yielded(_)) => executed += 1,
            Err(error) => return context.error(&format!("Task {id} ({class}): {error}")),
        }
    }
    log(
        context,
        GREEN,
        "DONE",
        &format!("Executed {executed} task(s)."),
    );
    0
}
