//! `/daemon/`, read-only: the console (`PhabricatorDaemonConsoleController`)
//! and a task (`PhabricatorWorkerTaskDetailController`), for
//! administrators, and the builtin bulk job queries
//! (`PhabricatorWorkerBulkJobSearchEngine`). Daemon logs and bulk job
//! pages stay with PHP.
//!
//! PHP's console downgrades the status of daemons it hasn't heard from and
//! saves it; this page computes the same status without writing it.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::has_named_queries;
use phab_core::Phid;
use phab_db::sqlx::Row as _;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};

use super::{
    Body, Item, Link, List, PageBuilder, Property, Section, SystemPage, Table, no_query,
    relative_time, relative_time_detailed, thousands,
};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::response::Page;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// `PhabricatorDaemonsApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorDaemonsApplication";

/// `PhabricatorWorkerBulkJobSearchEngine`.
const BULK_ENGINE: &str = "PhabricatorWorkerBulkJobSearchEngine";

/// `PhutilDaemonHandle::getHeartbeatEventFrequency()`.
const HEARTBEAT: i64 = 120;

/// `PhutilDaemonHandle::getWaitBeforeRestart()`.
const WAIT_BEFORE_RESTART: f64 = 5.0;

fn navigation(current: &str) -> Vec<Link> {
    vec![
        Link::new("Console", "/daemon/").inactive(current == "console"),
        Link::new("All Daemons", "/daemon/log/"),
        Link::new("Manage Bulk Jobs", "/daemon/bulk/").inactive(current == "bulk"),
    ]
}

/// A row of `worker_activetask` or `worker_archivetask`.
struct Task {
    id: i64,
    class: String,
    lease_owner: Option<String>,
    lease_expires: Option<i64>,
    failure_count: i64,
    priority: i64,
    data_id: i64,
    object_phid: Option<Phid>,
    /// Archived tasks: their result, duration and archive date.
    archived: Option<(i64, i64)>,
}

impl Task {
    fn from_row(row: &MySqlRow, archived: bool) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            class: rows::text(row, "taskClass")?,
            lease_owner: rows::opt_text(row, "leaseOwner")?,
            lease_expires: rows::opt_int(row, "leaseExpires")?,
            failure_count: rows::int(row, "failureCount")?,
            priority: rows::int(row, "priority")?,
            data_id: rows::int(row, "dataID")?,
            object_phid: rows::opt_phid(row, "objectPHID")?,
            archived: if archived {
                Some((rows::int(row, "result")?, rows::int(row, "duration")?))
            } else {
                None
            },
        })
    }
}

const TASK_COLUMNS: &str =
    "id, taskClass, leaseOwner, leaseExpires, failureCount, priority, dataID, objectPHID";

async fn active_tasks(
    database: &Database,
    condition: &str,
    order_limit: &str,
) -> Result<Vec<Task>, DbError> {
    let sql = format!(
        "SELECT {TASK_COLUMNS} FROM {} WHERE {condition} {order_limit}",
        database.table("worker", "worker_activetask")
    );
    phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| Task::from_row(row, false))
        .collect()
}

/// `PhabricatorDaemonTasksTableView`.
fn tasks_table(tasks: &[Task], now: i64, empty: &str) -> Table {
    Table {
        headers: vec![
            "ID", "Class", "Owner", "Expires", "Priority", "Failures", "",
        ],
        rows: tasks
            .iter()
            .map(|task| {
                vec![
                    task.id.to_string(),
                    escape(&task.class),
                    escape(task.lease_owner.as_deref().unwrap_or("")),
                    task.lease_expires
                        .filter(|expires| *expires != 0)
                        .map_or_else(|| "-".to_owned(), |expires| relative_time(expires - now)),
                    task.priority.to_string(),
                    task.failure_count.to_string(),
                    format!(
                        "<a class=\"hm-button\" href=\"/daemon/task/{}/\">View Task</a>",
                        task.id
                    ),
                ]
            })
            .collect(),
        empty: empty.to_owned(),
    }
}

/// The statistics of a task class over the last 15 minutes.
#[derive(Default)]
struct Completed {
    count: i64,
    duration: i64,
    queue_time: i64,
}

/// `GET /daemon/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn console<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    super::require_admin(state, &viewer, APPLICATION).await?;
    no_query(&uri)?;
    let database = state.database();
    let now = state.now();
    let window_start = now - 15 * 60;
    let lease_overhead = 0.250;

    // `PhabricatorWorkerArchiveTaskQuery::withDateModifiedSince()`, by ID.
    let sql = format!(
        "SELECT taskClass, duration, dateCreated, dateModified, archivedEpoch FROM {} \
         WHERE dateModified > ? ORDER BY id DESC",
        database.table("worker", "worker_archivetask")
    );
    let completed_rows = phab_db::sqlx::query(&sql)
        .bind(window_start)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let mut usage_total = 0.0;
    let mut usage_start = i64::MAX;
    let mut completed: Vec<(String, Completed)> = Vec::new();
    for row in &completed_rows {
        let class = rows::text(row, "taskClass")?;
        let duration = rows::int(row, "duration")?;
        let created = rows::int(row, "dateCreated")?;
        let modified = rows::int(row, "dateModified")?;
        let archived = rows::opt_int(row, "archivedEpoch")?.unwrap_or(0);
        let index = if let Some(index) = completed.iter().position(|(name, _)| *name == class) {
            index
        } else {
            completed.push((class, Completed::default()));
            completed.len() - 1
        };
        let info = &mut completed[index].1;
        info.count += 1;
        info.duration += duration;
        #[allow(clippy::cast_precision_loss)]
        {
            usage_total += lease_overhead + duration as f64 / 1_000_000.0;
        }
        usage_start = usage_start.min(modified);
        let queue_seconds = archived - created;
        if queue_seconds > 0 {
            #[allow(clippy::cast_precision_loss)]
            let exclusive = (queue_seconds * 1_000_000 - duration) as f64 / 1_000_000.0;
            #[allow(clippy::cast_possible_truncation)]
            {
                info.queue_time += exclusive.floor() as i64;
            }
        }
    }
    // `isort($completed_info, 'n')`: ascending count, stable.
    completed.sort_by_key(|(_, info)| info.count);
    let mut rows = Vec::new();
    for (class, info) in &completed {
        rows.push(vec![
            escape(class),
            thousands(info.count),
            format!("{} us", thousands(info.duration / info.count)),
            format!("{} s", thousands(info.queue_time / info.count)),
        ]);
    }
    let failed = active_tasks(
        database,
        &format!("failureTime > {window_start}"),
        "ORDER BY id",
    )
    .await?;
    if !failed.is_empty() {
        #[allow(clippy::cast_precision_loss)]
        {
            usage_total += WAIT_BEFORE_RESTART * failed.len() as f64 * 2.0;
        }
        let sql = format!(
            "SELECT MIN(failureTime) AS first FROM {} WHERE failureTime > ?",
            database.table("worker", "worker_activetask")
        );
        let first: Option<i64> = phab_db::sqlx::query(&sql)
            .bind(window_start)
            .fetch_one(database.pool())
            .await
            .map_err(DbError::from)?
            .try_get("first")
            .map_err(DbError::from)?;
        if let Some(first) = first {
            usage_start = usage_start.min(first);
        }
        rows.push(vec![
            "<em>Temporary Failures</em>".to_owned(),
            failed.len().to_string(),
            String::new(),
            String::new(),
        ]);
    }

    let daemons = alive_daemons(database, now).await?;
    let taskmasters = daemons
        .iter()
        .filter(|daemon| daemon.class == "PhabricatorTaskmasterDaemon")
        .count();
    if taskmasters > 0 && usage_total > 0.0 {
        #[allow(clippy::cast_precision_loss)]
        let available = taskmasters as f64 * (now - usage_start).max(15) as f64;
        rows.push(vec![
            "<em>Queue Utilization (Approximate)</em>".to_owned(),
            format!("{:.1}%", 100.0 * usage_total / available),
            String::new(),
            String::new(),
        ]);
    }
    let completed_table = Table {
        headers: vec!["Class", "Count", "Average Duration", "Average Queue Time"],
        rows,
        empty: "No tasks have completed in the last 15 minutes.".to_owned(),
    };

    let daemon_table = Table {
        headers: vec!["ID", "Host", "PPID", "Daemon", "Status", "Launched"],
        rows: daemons
            .iter()
            .map(|daemon| {
                let (icon, label) = daemon_status(&daemon.status);
                vec![
                    daemon.id.to_string(),
                    escape(&daemon.host),
                    daemon.pid.to_string(),
                    format!(
                        "<a href=\"/daemon/log/{}/\">{}</a>",
                        daemon.id,
                        escape(&daemon.class)
                    ),
                    format!("<span class=\"fa {icon}\" aria-hidden=\"true\"></span> {label}"),
                    escape(&time::format_utc(daemon.date_created)),
                ]
            })
            .collect(),
        empty: "No data available".to_owned(),
    };

    let sql = format!(
        "SELECT taskClass, COUNT(*) AS n FROM {} GROUP BY taskClass ORDER BY n DESC",
        database.table("worker", "worker_activetask")
    );
    let queued: Vec<Vec<String>> = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?
        .iter()
        .map(|row| {
            Ok(vec![
                escape(&rows::text(row, "taskClass")?),
                thousands(rows::int(row, "n")?),
            ])
        })
        .collect::<Result<_, DbError>>()?;

    let leased = active_tasks(
        database,
        &format!("leaseOwner IS NOT NULL AND leaseExpires >= {now}"),
        "ORDER BY priority ASC, id ASC LIMIT 100",
    )
    .await?;
    // `PhabricatorWorkerLeaseQuery` without a lease: unleased tasks, then
    // expired ones.
    let mut upcoming = active_tasks(
        database,
        "leaseOwner IS NULL",
        "ORDER BY priority ASC, id ASC LIMIT 10",
    )
    .await?;
    if upcoming.len() < 10 {
        let more = active_tasks(
            database,
            &format!("leaseExpires < {now}"),
            &format!("ORDER BY leaseExpires ASC LIMIT {}", 10 - upcoming.len()),
        )
        .await?;
        upcoming.extend(more);
    }

    let sql = format!(
        "SELECT t.id, t.clockClass, t.actionClass, e.lastEventEpoch, e.nextEventEpoch \
         FROM {} t JOIN {} e ON e.triggerID = t.id WHERE e.nextEventEpoch >= 0 \
         ORDER BY e.nextEventEpoch ASC, e.id ASC LIMIT 10",
        database.table("worker", "worker_trigger"),
        database.table("worker", "worker_triggerevent"),
    );
    let triggers: Vec<Vec<String>> = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?
        .iter()
        .map(|row| {
            let date = |epoch: Option<i64>| {
                epoch
                    .filter(|epoch| *epoch != 0)
                    .map_or_else(String::new, |epoch| escape(&time::format_utc(epoch)))
            };
            Ok(vec![
                rows::int(row, "id")?.to_string(),
                escape(&rows::text(row, "clockClass")?),
                escape(&rows::text(row, "actionClass")?),
                date(rows::opt_int(row, "lastEventEpoch")?),
                date(rows::opt_int(row, "nextEventEpoch")?),
            ])
        })
        .collect::<Result<_, DbError>>()?;

    Ok(PageBuilder::new("Console")
        .icon("fa-pied-piper-alt")
        .nav(navigation("console"))
        .section(Section::new(
            "Recently Completed Tasks (Last 15m)",
            Body::Table(completed_table),
        ))
        .section(Section::new("Active Daemons", Body::Table(daemon_table)))
        .section(Section::new(
            "Queued Tasks",
            Body::Table(Table {
                headers: vec!["Class", "Count"],
                rows: queued,
                empty: "Task queue is empty.".to_owned(),
            }),
        ))
        .section(Section::new(
            "Leased Tasks",
            Body::Table(tasks_table(&leased, now, "No tasks are leased by workers.")),
        ))
        .section(Section::new(
            "Next In Queue",
            Body::Table(tasks_table(&upcoming, now, "Task queue is empty.")),
        ))
        .section(Section::new(
            "Upcoming Triggers",
            Body::Table(Table {
                headers: vec!["ID", "Clock", "Action", "Last", "Next"],
                rows: triggers,
                empty: "There are no upcoming event triggers.".to_owned(),
            }),
        ))
        .build(state, &viewer, &uri))
}

/// A `daemon_log` row.
struct Daemon {
    id: i64,
    class: String,
    host: String,
    pid: i64,
    status: String,
    date_created: i64,
}

/// `PhabricatorDaemonLogQuery` with `STATUS_ALIVE`: the status downgraded
/// for daemons not heard from (`willFilterPage()`), without saving it.
async fn alive_daemons(database: &Database, now: i64) -> Result<Vec<Daemon>, DbError> {
    let sql = format!(
        "SELECT id, daemon, host, pid, status, dateCreated, dateModified FROM {} \
         WHERE status IN ('unknown', 'run', 'wait', 'exiting') ORDER BY id DESC",
        database.table("daemon", "daemon_log")
    );
    let mut daemons = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await?
    {
        let mut status = rows::text(&row, "status")?;
        let seen = rows::int(&row, "dateModified")?;
        let running = matches!(status.as_str(), "run" | "wait" | "exiting");
        if running && seen + 3 * HEARTBEAT < now {
            "unknown".clone_into(&mut status);
        }
        if status == "unknown" && seen + 30 * HEARTBEAT < now {
            continue;
        }
        daemons.push(Daemon {
            id: rows::int(&row, "id")?,
            class: rows::text(&row, "daemon")?,
            host: rows::text(&row, "host")?,
            pid: rows::int(&row, "pid")?,
            status,
            date_created: rows::int(&row, "dateCreated")?,
        });
    }
    Ok(daemons)
}

/// `PhabricatorDaemonLogListView`'s icon and label of a status.
fn daemon_status(status: &str) -> (&'static str, &'static str) {
    match status {
        "run" => ("fa-rocket remarkup-icon-green", "Running"),
        "dead" => ("fa-warning remarkup-icon-red", "Dead"),
        "exiting" => ("fa-check", "Shutting Down"),
        "exit" => ("fa-check", "Exited"),
        "wait" => ("fa-clock-o remarkup-icon-blue", "Waiting"),
        _ => ("fa-warning remarkup-icon-orange", "Unknown"),
    }
}

/// The wait before a retry, given the failure count.
type WaitBeforeRetry = fn(i64) -> Option<i64>;

/// `getMaximumRetryCount()` and `getWaitBeforeRetry()` of the task
/// classes which override them.
fn retry_policy(class: &str) -> (Option<i64>, WaitBeforeRetry) {
    match class {
        "PhabricatorMetaMTAWorker" => (Some(250), |failures| Some(failures * 15)),
        "FeedPublisherHTTPWorker" => (None, |failures| Some(failures.max(1) * 60)),
        "ReviewHelperAutoRequestWorker" | "RevisionMergeConflictWorker" => (Some(2), |_| None),
        _ => (None, |_| None),
    }
}

/// The task classes whose `renderForDisplay()` links objects, or which
/// subclass a class overriding the retries.
fn displays_data_of_its_own(class: &str) -> bool {
    matches!(
        class,
        "HarbormasterBuildWorker"
            | "HarbormasterTargetWorker"
            | "PhabricatorRepositoryCommitParserWorker"
            | "PhabricatorRepositoryCommitPublishWorker"
            | "PhabricatorRepositoryGitCommitMessageParserWorker"
            | "PhabricatorRepositoryGitCommitChangeParserWorker"
            | "PhabricatorRepositoryMercurialCommitMessageParserWorker"
            | "PhabricatorRepositoryMercurialCommitChangeParserWorker"
            | "PhabricatorRepositorySvnCommitMessageParserWorker"
            | "PhabricatorRepositorySvnCommitChangeParserWorker"
            | "PhabricatorRepositoryCommitHeraldWorker"
            | "PhabricatorRepositoryCommitOwnersWorker"
    ) || class.starts_with("FeedPublisher")
        && class != "FeedPublisherWorker"
        && class != "FeedPublisherHTTPWorker"
}

/// `GET /daemon/task/<id>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn task<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    super::require_admin(state, &viewer, APPLICATION).await?;
    no_query(&uri)?;
    let id = super::parse_id(&id)?;
    let database = state.database();
    let now = state.now();
    let mut task = active_tasks(database, &format!("id = {id}"), "")
        .await?
        .pop();
    if task.is_none() {
        let sql = format!(
            "SELECT {TASK_COLUMNS}, result, duration FROM {} WHERE id = ?",
            database.table("worker", "worker_archivetask")
        );
        task = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await
            .map_err(DbError::from)?
            .as_ref()
            .map(|row| Task::from_row(row, true))
            .transpose()?;
    }
    let Some(task) = task else {
        return Ok(PageBuilder::new(format!("Task {id} Missing"))
            .title("Task Does Not Exist")
            .icon("fa-sort")
            .section(Section::new(
                "No Such Task",
                Body::Notice("This task may have recently been garbage collected.".to_owned()),
            ))
            .build(state, &viewer, &uri));
    };
    if displays_data_of_its_own(&task.class) {
        return Err(PageError::Fallback("the data of a task class not ported"));
    }

    let mut properties = Vec::new();
    if let Some(object) = &task.object_phid {
        let handles = harbormaster::load_handles(
            state,
            &Scope::new(state, &viewer),
            std::slice::from_ref(object),
        )
        .await?;
        let handle = harbormaster::handle(&handles, object);
        // `isComplete()`: the object exists and the viewer can see it.
        if handle.uri.is_some() {
            properties.push(Property::html("Object", handle.link_html()));
        }
    }
    let status = match task.archived {
        None => "Queued",
        Some((0, _)) => "Complete",
        Some((1, _)) => "Failed",
        Some((2, _)) => "Cancelled",
        Some(_) => return Err(PageError::Fallback("a task of an unknown status")),
    };
    properties.push(Property::text("Task Status", status));
    properties.push(Property::text("Task Class", &task.class));
    let expires = task.lease_expires.filter(|expires| *expires != 0);
    properties.push(match expires {
        Some(expires) if expires > now => Property::text("Lease Status", "Leased"),
        Some(_) => Property::text("Lease Status", "Lease Expired"),
        None => Property::html("Lease Status", "<em>Not Leased</em>".to_owned()),
    });
    let owner = task
        .lease_owner
        .as_deref()
        .filter(|owner| !owner.is_empty());
    properties.push(match owner {
        Some(owner) => Property::text("Lease Owner", owner),
        None => Property::html("Lease Owner", "<em>None</em>".to_owned()),
    });
    properties.push(match (expires, owner) {
        (Some(expires), Some(_)) => {
            Property::text("Lease Expires", &relative_time_detailed(expires - now))
        }
        _ => Property::html("Lease Expires", "<em>None</em>".to_owned()),
    });
    properties.push(match task.archived {
        Some((_, duration)) => Property::text("Duration", &format!("{} us", thousands(duration))),
        None => Property::html("Duration", "<em>Not Completed</em>".to_owned()),
    });
    if task.class == "PhabricatorMetaMTAWorker" {
        let data = task_data(database, task.data_id).await?;
        properties.push(Property::html(
            "Data",
            format!(
                "<pre>phabricator/ $ ./bin/mail show-outbound --id {}</pre>",
                escape(&phab_core::php::string_cast(&data))
            ),
        ));
    }

    let (maximum, wait) = retry_policy(&task.class);
    let mut retries = vec![Property::text(
        "Failure Count",
        &task.failure_count.to_string(),
    )];
    retries.push(match maximum {
        Some(maximum) => Property::text("Maximum Retries", &maximum.to_string()),
        None => Property::html("Maximum Retries", "<em>Retries Forever</em>".to_owned()),
    });
    let limit = maximum.unwrap_or(i64::MAX);
    let mut next = Vec::new();
    let mut failures = task.failure_count;
    while failures < limit {
        next.push(wait(failures).unwrap_or(60));
        failures += 1;
        if next.len() > 10 {
            break;
        }
    }
    let retries_after = if next.is_empty() {
        "No More Retries".to_owned()
    } else {
        let mut cumulative = 0;
        let mut parts: Vec<String> = next
            .iter()
            .map(|duration| {
                cumulative += duration;
                relative_time(cumulative)
            })
            .collect();
        if failures != limit {
            parts.push("...".to_owned());
        }
        parts.join(", ")
    };
    retries.push(Property::text("Retries After", &retries_after));

    let title = format!("Task {}", task.id);
    Ok(
        PageBuilder::new(format!("Task {}: {}", task.id, task.class))
            .title(title.clone())
            .icon("fa-sort")
            .section(Section::new(title, Body::Properties(properties)))
            .section(Section::new("Retries", Body::Properties(retries)))
            .build(state, &viewer, &uri),
    )
}

/// `worker_taskdata.data`, decoded.
async fn task_data(database: &Database, id: i64) -> Result<serde_json::Value, PageError> {
    let sql = format!(
        "SELECT data FROM {} WHERE id = ?",
        database.table("worker", "worker_taskdata")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?;
    Ok(match row {
        Some(row) => rows::json(&row, "data")?,
        None => serde_json::Value::Null,
    })
}

/// `GET /daemon/bulk/`.
pub(crate) async fn bulk<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if has_named_queries(state.database(), &viewer.user.phid, BULK_ENGINE).await? {
        return Err(PageError::Fallback("saved bulk job queries"));
    }
    // The first builtin query: "Authored Jobs" for logged-in viewers.
    bulk_list(state.as_ref(), &viewer, "authored", &uri).await
}

/// `GET /daemon/bulk/query/<key>/`.
pub(crate) async fn bulk_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "authored" => "authored",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved bulk job query")),
    };
    bulk_list(state.as_ref(), &viewer, key, &uri).await
}

/// The jobs of a builtin query, newest first, a page of 100.
async fn bulk_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    uri: &Uri,
) -> Result<Page<SystemPage>, PageError> {
    no_query(uri)?;
    let database = state.database();
    let author = if key == "authored" {
        "WHERE authorPHID = ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, jobTypeKey, status, size, dateCreated FROM {} {author} \
         ORDER BY id DESC LIMIT 101",
        database.table("worker", "worker_bulkjob")
    );
    let mut query = phab_db::sqlx::query(&sql);
    if key == "authored" {
        query = query.bind(viewer.user.phid.as_str());
    }
    let found = query
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    if found.len() > 100 {
        return Err(PageError::Fallback("more than a page of bulk jobs"));
    }
    let mut items = Vec::new();
    for row in &found {
        let id = rows::int(row, "id")?;
        let name = match rows::text(row, "jobTypeKey")?.as_str() {
            "transaction.edit" => "Bulk Edit",
            "export" => "Data Export",
            _ => return Err(PageError::Fallback("a bulk job type not ported")),
        };
        let status = rows::text(row, "status")?;
        let (icon, label) = match status.as_str() {
            "confirm" => ("fa-question", "Confirming"),
            "waiting" => ("fa-clock-o", "Waiting"),
            "running" => ("fa-clock-o", "Running"),
            "complete" => ("fa-check", "Complete"),
            _ => ("fa-question", status.as_str()),
        };
        let size = rows::int(row, "size")?;
        items.push(Item {
            name: format!("Bulk Job {id}: {name}"),
            uri: Some(format!("/daemon/bulk/view/{id}/")),
            icon: Some(icon.to_owned()),
            tags: vec![
                label.to_owned(),
                format!("{} Bulk Task(s)", thousands(size)),
            ],
            attributes_html: vec![escape(&time::format_utc(rows::int(row, "dateCreated")?))],
            disabled: false,
        });
    }
    let title = if key == "authored" {
        "Authored Jobs"
    } else {
        "All Jobs"
    };
    Ok(PageBuilder::new("Bulk Jobs")
        .title(title)
        .icon("fa-hourglass-1")
        .nav(vec![
            Link::new("Authored Jobs", "/daemon/bulk/query/authored/").inactive(key == "authored"),
            Link::new("All Jobs", "/daemon/bulk/query/all/").inactive(key == "all"),
        ])
        .section(Section::untitled(Body::List(List {
            items,
            empty: "No bulk jobs found.".to_owned(),
        })))
        .build(state, viewer, uri))
}
