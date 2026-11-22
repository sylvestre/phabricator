//! `PhabricatorWorkerLeaseQuery`: claims tasks from `worker_activetask`.
//!
//! The query is PHP's, with one more condition: `taskClass IN (...)`, the
//! classes the worker has handlers for. Both the `SELECT` and the
//! `UPDATE ... SET leaseOwner` of each phase only ever see those classes, so
//! a task of any other class is never leased, not even for a moment, and
//! PHP's taskmasters find the queue as they would without this worker.

use std::sync::atomic::{AtomicU64, Ordering};

use phab_db::{Database, DbError, bind_all, placeholders, rows};

use crate::task::{ActiveTask, decode_data};
use crate::time;

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
#[derive(Debug, Clone)]
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
}

/// A task selected in a phase: `SELECT id, leaseOwner`.
struct Candidate {
    id: i64,
    lease_owner: Option<String>,
}

impl LeaseQuery<'_> {
    /// `buildCustomWhereClause()`, plus the class filter.
    fn where_clause(&self, phase: Phase) -> String {
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
        format!("WHERE {}", conditions.join(" AND "))
    }

    async fn select(
        &self,
        database: &Database,
        phase: Phase,
        limit: usize,
    ) -> Result<Vec<Candidate>, DbError> {
        let sql = format!(
            "SELECT id, leaseOwner FROM {} {} {} LIMIT {limit}",
            database.table("worker", "worker_activetask"),
            self.where_clause(phase),
            phase.order(),
        );
        let mut query = bind_all(sqlx::query(&sql), self.classes.iter().copied());
        if let Some(ids) = self.ids {
            query = bind_all(query, ids.iter().copied());
        }
        if let Some(phids) = self.object_phids {
            query = bind_all(query, phids.iter().map(String::as_str));
        }
        query
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| {
                Ok(Candidate {
                    id: rows::int(row, "id")?,
                    lease_owner: rows::opt_text(row, "leaseOwner")?,
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
                        .bind(candidate.lease_owner.as_deref().unwrap_or(""));
                }
                query.execute(database.pool()).await?
            }
        };
        Ok(result.rows_affected())
    }

    /// `execute()` without `setSkipLease()`: leases up to `limit` new tasks,
    /// then tasks with an expired lease, under a fresh name from `owner`,
    /// and loads the tasks this name holds with their data.
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
        for phase in [Phase::Unleased, Phase::Expired] {
            last_phase = phase;
            let remaining = usize::try_from(limit - leased).unwrap_or(usize::MAX);
            let candidates = self.select(database, phase, remaining).await?;
            if candidates.is_empty() {
                continue;
            }
            leased += Self::lease(database, phase, &candidates, &name).await?;
            if leased == limit {
                break;
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
                Ok(ActiveTask {
                    id: rows::int(row, "id")?,
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
        };
        assert_eq!(
            query.where_clause(Phase::Unleased),
            "WHERE leaseOwner IS NULL AND taskClass IN (?, ?) AND id IN (?)"
        );
        assert_eq!(
            query.where_clause(Phase::Expired),
            "WHERE leaseExpires < UNIX_TIMESTAMP() AND taskClass IN (?, ?) AND id IN (?)"
        );
    }
}
