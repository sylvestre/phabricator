//! `PhabricatorWorkerLeaseQuery`: claims tasks from `worker_activetask`.
//!
//! The query is PHP's, with one more condition: `taskClass IN (...)`, the
//! classes the worker has handlers for. Both the `SELECT` and the
//! `UPDATE ... SET leaseOwner` of each phase only ever see those classes, so
//! a task of any other class is never leased, not even for a moment, and
//! PHP's taskmasters find the queue as they would without this worker.
//!
//! A class may be ported for only some of its tasks. With an
//! [`Inspection`], the query shows each task it selects to its handler
//! before the `UPDATE` ([`crate::Handler::inspect`]); a task the handler
//! declines is not leased, so it stays exactly as PHP left it and PHP's
//! next lease query takes it in its usual order. The worker remembers
//! declined tasks for a while ([`Declined`]) and selects past them, so they
//! neither block the tasks it can run nor get inspected on every query.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use phab_core::Config;
use phab_db::{Database, DbError, bind_all, placeholders, rows};
use serde_json::Value;

use crate::handler::{Claim, Registry, Unleased};
use crate::task::{ActiveTask, PriorLease, decode_data};
use crate::time;

/// How long a declined task is selected past before it is inspected again.
pub const DECLINE_MEMORY: Duration = Duration::from_secs(10 * 60);

/// The most declined tasks remembered; the oldest are forgotten first.
const MAXIMUM_DECLINED: usize = 1000;

/// The tasks handlers declined recently, with why.
#[derive(Debug, Default)]
pub struct Declined {
    tasks: Mutex<HashMap<i64, (Instant, String)>>,
}

impl Declined {
    fn tasks(&self) -> std::sync::MutexGuard<'_, HashMap<i64, (Instant, String)>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The tasks to select past, forgetting those declined too long ago.
    pub fn ids(&self) -> Vec<i64> {
        let mut tasks = self.tasks();
        tasks.retain(|_, (declined, _)| declined.elapsed() < DECLINE_MEMORY);
        let mut ids: Vec<i64> = tasks.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Remembers that task `id` was declined.
    pub fn insert(&self, id: i64, reason: String) {
        let mut tasks = self.tasks();
        if tasks.len() >= MAXIMUM_DECLINED
            && !tasks.contains_key(&id)
            && let Some(oldest) = tasks
                .iter()
                .min_by_key(|(_, (declined, _))| *declined)
                .map(|(id, _)| *id)
        {
            tasks.remove(&oldest);
        }
        tasks.insert(id, (Instant::now(), reason));
    }

    /// Why task `id` was declined, if it was recently.
    pub fn reason(&self, id: i64) -> Option<String> {
        self.tasks()
            .get(&id)
            .filter(|(declined, _)| declined.elapsed() < DECLINE_MEMORY)
            .map(|(_, reason)| reason.clone())
    }
}

/// What lets a lease query ask handlers about the tasks it selects.
#[derive(Clone, Copy)]
pub struct Inspection<'a> {
    /// The handlers, by class.
    pub registry: &'a Registry,
    /// The databases, for the handlers.
    pub database: &'a Database,
    /// The configuration, for the handlers.
    pub config: &'a Config,
    /// Where declines are remembered.
    pub declined: &'a Declined,
}

/// `PhabricatorWorkerLeaseQuery::getDefaultLeaseDuration()`: 2 hours.
pub const DEFAULT_LEASE_DURATION: i64 = 2 * 60 * 60;

/// `PhabricatorWorkerLeaseQuery::getDefaultWaitBeforeRetry()`: 5 minutes.
pub const DEFAULT_WAIT_BEFORE_RETRY: i64 = 5 * 60;

/// The longest host name part of a lease owner, in bytes.
const MAXIMUM_HOST_BYTES: usize = 32;

/// `getLeaseOwnershipName()`: `pid:time:host:sequence`, a new name for every
/// lease query, so the final `SELECT` finds exactly the tasks it leased.
#[derive(Debug)]
pub struct LeaseOwner {
    pid: u32,
    host: String,
    sequence: AtomicU64,
}

/// The host name cut to 32 bytes with `...`, on a character boundary.
///
/// PHP's `PhutilUTF8StringTruncator` may also cut earlier, at a word break.
/// Nothing parses lease owners, so only the length limit (the column is
/// `varchar(64)`) matters.
fn truncate_host(host: &str) -> String {
    if host.len() <= MAXIMUM_HOST_BYTES {
        return host.to_owned();
    }
    let mut end = MAXIMUM_HOST_BYTES - "...".len();
    while !host.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &host[..end])
}

impl LeaseOwner {
    /// Lease owners for this process, on `host` (`php_uname('n')`).
    pub fn new(host: &str) -> Self {
        Self {
            pid: std::process::id(),
            host: truncate_host(host),
            sequence: AtomicU64::new(0),
        }
    }

    /// The next name (`++$sequence`).
    pub fn next_name(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}:{}:{}:{sequence}", self.pid, time(), self.host)
    }
}

/// The phases of a lease query, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `PHASE_UNLEASED`: tasks nobody ever leased.
    Unleased,
    /// `PHASE_EXPIRED`: tasks whose lease ran out (failed, yielded, or
    /// held by a worker which died).
    Expired,
}

impl Phase {
    fn condition(self) -> &'static str {
        match self {
            Self::Unleased => "leaseOwner IS NULL",
            Self::Expired => "leaseExpires < UNIX_TIMESTAMP()",
        }
    }

    /// `buildOrderClause()`: new tasks by priority then FIFO, failed ones
    /// in the order they failed, so a blocked task goes to the back.
    fn order(self) -> &'static str {
        match self {
            Self::Unleased => "ORDER BY priority ASC, id ASC",
            Self::Expired => "ORDER BY leaseExpires ASC",
        }
    }
}

/// A lease query; [`LeaseQuery::execute`] claims at most `limit` tasks.
#[derive(Clone)]
pub struct LeaseQuery<'a> {
    /// Only these task classes (the registered handlers). Empty leases
    /// nothing.
    pub classes: &'a [&'a str],
    /// `withIDs()`.
    pub ids: Option<&'a [i64]>,
    /// `withObjectPHIDs()`.
    pub object_phids: Option<&'a [String]>,
    /// `setLimit()`.
    pub limit: usize,
    /// Asks the handlers before leasing; `None` leases whatever the query
    /// selects, as PHP does.
    pub inspection: Option<Inspection<'a>>,
}

/// A task selected in a phase: `SELECT id, leaseOwner`, and what the
/// inspection and [`ActiveTask::release`] need.
struct Candidate {
    id: i64,
    task_class: String,
    data_id: i64,
    prior: PriorLease,
}

impl Inspection<'_> {
    /// The candidates the handlers take; remembers the others.
    async fn claim(&self, candidates: Vec<Candidate>) -> Result<Vec<Candidate>, DbError> {
        let mut data_ids: Vec<i64> = candidates.iter().map(|task| task.data_id).collect();
        data_ids.sort_unstable();
        data_ids.dedup();
        let sql = format!(
            "SELECT id, data FROM {} WHERE id IN ({})",
            self.database.table("worker", "worker_taskdata"),
            placeholders(data_ids.len())
        );
        let mut data: HashMap<i64, Value> = HashMap::new();
        for row in bind_all(sqlx::query(&sql), data_ids.iter().copied())
            .fetch_all(self.database.pool())
            .await?
        {
            data.insert(
                rows::int(&row, "id")?,
                decode_data(rows::opt_text(&row, "data")?.as_deref()),
            );
        }
        let mut taken = Vec::new();
        for candidate in candidates {
            let Some(handler) = self.registry.get(&candidate.task_class) else {
                taken.push(candidate);
                continue;
            };
            let task = Unleased {
                id: candidate.id,
                class: &candidate.task_class,
                data: data.get(&candidate.data_id).unwrap_or(&Value::Null),
                database: self.database,
                config: self.config,
            };
            match handler.inspect(&task).await {
                Claim::Take => taken.push(candidate),
                Claim::Decline(reason) => {
                    let (id, class) = (candidate.id, &candidate.task_class);
                    tracing::info!(
                        id,
                        class,
                        reason,
                        "Task {id} ({class}) left to PHP: {reason}"
                    );
                    self.declined.insert(candidate.id, reason);
                }
            }
        }
        Ok(taken)
    }
}

impl LeaseQuery<'_> {
    /// `buildCustomWhereClause()`, plus the class filter, and the declined
    /// tasks to select past.
    fn where_clause(&self, phase: Phase, excluded: &[i64]) -> String {
        let mut conditions = vec![phase.condition().to_owned()];
        conditions.push(format!(
            "taskClass IN ({})",
            placeholders(self.classes.len())
        ));
        if let Some(ids) = self.ids {
            conditions.push(format!("id IN ({})", placeholders(ids.len())));
        }
        if let Some(phids) = self.object_phids {
            conditions.push(format!("objectPHID IN ({})", placeholders(phids.len())));
        }
        if !excluded.is_empty() {
            conditions.push(format!("id NOT IN ({})", placeholders(excluded.len())));
        }
        format!("WHERE {}", conditions.join(" AND "))
    }

    async fn select(
        &self,
        database: &Database,
        phase: Phase,
        limit: usize,
        excluded: &[i64],
    ) -> Result<Vec<Candidate>, DbError> {
        let sql = format!(
            "SELECT id, leaseOwner, leaseExpires, dateModified, taskClass, dataID FROM {} {} {} \
             LIMIT {limit}",
            database.table("worker", "worker_activetask"),
            self.where_clause(phase, excluded),
            phase.order(),
        );
        let mut query = bind_all(sqlx::query(&sql), self.classes.iter().copied());
        if let Some(ids) = self.ids {
            query = bind_all(query, ids.iter().copied());
        }
        if let Some(phids) = self.object_phids {
            query = bind_all(query, phids.iter().map(String::as_str));
        }
        query = bind_all(query, excluded.iter().copied());
        query
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| {
                Ok(Candidate {
                    id: rows::int(row, "id")?,
                    task_class: rows::text(row, "taskClass")?,
                    data_id: rows::int(row, "dataID")?,
                    prior: PriorLease {
                        owner: rows::opt_text(row, "leaseOwner")?,
                        expires: rows::opt_int(row, "leaseExpires")?,
                        date_modified: rows::int(row, "dateModified")?,
                    },
                })
            })
            .collect()
    }

    /// The `UPDATE` of a phase, with `buildUpdateWhereClause()`: it only
    /// takes rows still in the state they were selected in, so a race with
    /// another worker leases each task once. Returns the rows leased.
    async fn lease(
        database: &Database,
        phase: Phase,
        candidates: &[Candidate],
        owner: &str,
    ) -> Result<u64, DbError> {
        let table = database.table("worker", "worker_activetask");
        let set = format!(
            "UPDATE {table} task SET leaseOwner = ?, \
             leaseExpires = UNIX_TIMESTAMP() + {DEFAULT_LEASE_DURATION}"
        );
        let result = match phase {
            Phase::Unleased => {
                let sql = format!(
                    "{set} WHERE leaseOwner IS NULL AND id IN ({})",
                    placeholders(candidates.len())
                );
                bind_all(
                    sqlx::query(&sql).bind(owner),
                    candidates.iter().map(|candidate| candidate.id),
                )
                .execute(database.pool())
                .await?
            }
            Phase::Expired => {
                let sql = format!(
                    "{set} WHERE ({})",
                    vec!["(id = ? AND leaseOwner = ?)"; candidates.len()].join(" OR ")
                );
                let mut query = sqlx::query(&sql).bind(owner);
                for candidate in candidates {
                    // `%s` of `null` is `''`, which no row has either.
                    query = query
                        .bind(candidate.id)
                        .bind(candidate.prior.owner.as_deref().unwrap_or(""));
                }
                query.execute(database.pool()).await?
            }
        };
        Ok(result.rows_affected())
    }

    /// `execute()` without `setSkipLease()`: leases up to `limit` new tasks,
    /// then tasks with an expired lease, under a fresh name from `owner`,
    /// and loads the tasks this name holds with their data.
    ///
    /// With an inspection, a phase selects again past the tasks it
    /// declined, until it declines none or finds nothing more.
    pub async fn execute(
        &self,
        database: &Database,
        owner: &LeaseOwner,
    ) -> Result<Vec<ActiveTask>, DbError> {
        assert!(self.limit > 0, "You must setLimit() when leasing tasks.");
        if self.classes.is_empty()
            || self.ids.is_some_and(<[i64]>::is_empty)
            || self.object_phids.is_some_and(<[String]>::is_empty)
        {
            // `IN ()` is a syntax error; nothing can match.
            return Ok(Vec::new());
        }
        let name = owner.next_name();
        let mut leased: u64 = 0;
        let limit = self.limit as u64;
        let mut last_phase = Phase::Unleased;
        let mut priors: HashMap<i64, PriorLease> = HashMap::new();
        'phases: for phase in [Phase::Unleased, Phase::Expired] {
            last_phase = phase;
            loop {
                let remaining = usize::try_from(limit - leased).unwrap_or(usize::MAX);
                let excluded = self
                    .inspection
                    .map(|inspection| inspection.declined.ids())
                    .unwrap_or_default();
                let selected = self.select(database, phase, remaining, &excluded).await?;
                if selected.is_empty() {
                    break;
                }
                let count = selected.len();
                let candidates = match &self.inspection {
                    Some(inspection) => inspection.claim(selected).await?,
                    None => selected,
                };
                let declined_some = candidates.len() < count;
                if !candidates.is_empty() {
                    leased += Self::lease(database, phase, &candidates, &name).await?;
                    priors.extend(
                        candidates
                            .into_iter()
                            .map(|candidate| (candidate.id, candidate.prior)),
                    );
                }
                if leased == limit {
                    break 'phases;
                }
                if !declined_some {
                    break;
                }
            }
        }
        if leased == 0 {
            return Ok(Vec::new());
        }

        let sql = format!(
            "SELECT task.*, taskdata.data _taskData, UNIX_TIMESTAMP() _serverTime \
             FROM {} task LEFT JOIN {} taskdata ON taskdata.id = task.dataID \
             WHERE task.leaseOwner = ? AND leaseExpires > UNIX_TIMESTAMP() {} LIMIT {}",
            database.table("worker", "worker_activetask"),
            database.table("worker", "worker_taskdata"),
            last_phase.order(),
            self.limit,
        );
        let rows = sqlx::query(&sql)
            .bind(&name)
            .fetch_all(database.pool())
            .await?;
        let local_time = time();
        rows.iter()
            .map(|row| {
                let id = rows::int(row, "id")?;
                Ok(ActiveTask {
                    id,
                    task_class: rows::text(row, "taskClass")?,
                    lease_owner: rows::opt_text(row, "leaseOwner")?,
                    lease_expires: rows::opt_int(row, "leaseExpires")?,
                    failure_count: rows::int(row, "failureCount")?,
                    data_id: rows::int(row, "dataID")?,
                    failure_time: rows::opt_int(row, "failureTime")?,
                    priority: rows::int(row, "priority")?,
                    object_phid: rows::opt_text(row, "objectPHID")?,
                    container_phid: rows::opt_text(row, "containerPHID")?,
                    date_created: rows::int(row, "dateCreated")?,
                    date_modified: rows::int(row, "dateModified")?,
                    data: decode_data(rows::opt_text(row, "_taskData")?.as_deref()),
                    server_time: rows::int(row, "_serverTime")?,
                    local_time,
                    prior_lease: priors.remove(&id).unwrap_or_default(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_host_names() {
        assert_eq!(truncate_host("web1"), "web1");
        let exact = "a".repeat(32);
        assert_eq!(truncate_host(&exact), exact);
        let long = format!("{}.example.com", "b".repeat(30));
        assert_eq!(truncate_host(&long), format!("{}...", "b".repeat(29)));
        let wide = "\u{e9}".repeat(20);
        let truncated = truncate_host(&wide);
        assert!(truncated.len() <= 32);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn names_change_with_every_query() {
        let owner = LeaseOwner::new("host");
        let first = owner.next_name();
        let second = owner.next_name();
        assert!(first.ends_with(":host:1"), "{first}");
        assert!(second.ends_with(":host:2"), "{second}");
        assert!(first.starts_with(&format!("{}:", std::process::id())));
    }

    #[test]
    fn filters_on_the_handled_classes() {
        let query = LeaseQuery {
            classes: &["A", "B"],
            ids: Some(&[1]),
            object_phids: None,
            limit: 1,
            inspection: None,
        };
        assert_eq!(
            query.where_clause(Phase::Unleased, &[]),
            "WHERE leaseOwner IS NULL AND taskClass IN (?, ?) AND id IN (?)"
        );
        assert_eq!(
            query.where_clause(Phase::Expired, &[4, 5]),
            "WHERE leaseExpires < UNIX_TIMESTAMP() AND taskClass IN (?, ?) AND id IN (?) \
             AND id NOT IN (?, ?)"
        );
    }

    #[test]
    fn remembers_declined_tasks() {
        let declined = Declined::default();
        declined.insert(7, "not ported".to_owned());
        declined.insert(3, "no".to_owned());
        assert_eq!(declined.ids(), [3, 7]);
        assert_eq!(declined.reason(7).as_deref(), Some("not ported"));
        assert_eq!(declined.reason(8), None);
        for id in 100..(100 + i64::try_from(MAXIMUM_DECLINED).unwrap()) {
            declined.insert(id, String::new());
        }
        assert_eq!(declined.ids().len(), MAXIMUM_DECLINED);
        let newest = 99 + i64::try_from(MAXIMUM_DECLINED).unwrap();
        assert!(declined.reason(newest).is_some());
    }
}
