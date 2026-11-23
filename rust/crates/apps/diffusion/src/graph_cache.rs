//! `PhabricatorRepositoryGraphCache`: the commit which last changed a
//! path, found by walking first parents through the imported path
//! changes (`loadLastModifiedCommitID()`).
//!
//! The buckets of 4096 commits come from PHP's database cache
//! (`cache_general`) when it has them, and are rebuilt from
//! `repository_commit`, `repository_pathchange` and `repository_parents`
//! otherwise, as PHP does. Rust doesn't write the cache back.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use phab_conduit::{ApiRequest, MethodError};
use phab_db::phpser::{self, Key, Value};
use phab_db::rows;

use crate::cache;

/// `getBucketSize()`.
const BUCKET_SIZE: i64 = 4096;
/// `PhabricatorRepositoryCommit::IMPORTED_CHANGE`.
const IMPORTED_CHANGE: i64 = 2;
/// `DifferentialChangeType::TYPE_CHILD`.
const TYPE_CHILD: i64 = 10;

/// A commit of a bucket: its parents, and the sorted IDs of the paths it
/// changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    parents: Vec<i64>,
    paths: Vec<i64>,
}

type Bucket = HashMap<i64, Entry>;

/// What the cache knows of a path at a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The ID of the commit which last changed the path.
    Commit(i64),
    /// The walk reached a commit without parents (PHP's `null`).
    Root,
    /// The cache can't say (PHP's `false`): the commit isn't imported, or
    /// the walk took too long.
    Unknown,
}

/// The cache of one request (PHP's in-request level).
#[derive(Debug, Default)]
pub struct GraphCache {
    buckets: HashMap<i64, Bucket>,
    rebuilt: HashSet<i64>,
}

/// `getBucketCacheKey()`.
fn cache_key(bucket: i64) -> String {
    format!("PhabricatorRepositoryGraphCache:{BUCKET_SIZE}:2:{bucket}")
}

/// A bucket as PHP serializes it: commit IDs to their parents, `null`
/// and their paths. `None` for data PHP's `@unserialize()` would refuse
/// or find empty.
fn parse_bucket(data: &[u8]) -> Option<Bucket> {
    let Ok(Value::Array(entries)) = phpser::unserialize(data) else {
        return None;
    };
    let mut bucket = Bucket::new();
    for (key, value) in entries {
        let (Key::Int(commit), Value::Array(items)) = (key, value) else {
            return None;
        };
        let mut entry = Entry {
            parents: Vec::new(),
            paths: Vec::new(),
        };
        let mut after_null = false;
        for (_, item) in items {
            match (item, after_null) {
                (Value::Null, false) => after_null = true,
                (Value::Int(id), false) => entry.parents.push(id),
                (Value::Int(id), true) => entry.paths.push(id),
                _ => return None,
            }
        }
        if !after_null {
            return None;
        }
        bucket.insert(commit, entry);
    }
    (!bucket.is_empty()).then_some(bucket)
}

impl GraphCache {
    /// `rebuildBucket()`: the imported commits of the bucket which
    /// `current` doesn't have, once per request.
    async fn rebuild(
        &mut self,
        request: &ApiRequest<'_>,
        bucket: i64,
        mut current: Bucket,
    ) -> Result<Bucket, MethodError> {
        if !self.rebuilt.insert(bucket) {
            return Ok(current);
        }
        let database = &request.context.database;
        let minimum = bucket * BUCKET_SIZE;
        let sql = format!(
            "SELECT c.id FROM {} c JOIN {} r ON c.repositoryID = r.id \
             AND r.versionControlSystem IN ('git', 'hg') \
             WHERE c.id BETWEEN ? AND ? AND (c.importStatus & ?) = ?",
            database.table("repository", "repository_commit"),
            database.table("repository", "repository"),
        );
        let found = phab_db::sqlx::query(&sql)
            .bind(minimum)
            .bind(minimum + BUCKET_SIZE - 1)
            .bind(IMPORTED_CHANGE)
            .bind(IMPORTED_CHANGE)
            .fetch_all(database.pool())
            .await?;
        let mut ids = Vec::new();
        for row in &found {
            let id = rows::int(row, "id")?;
            if !current.contains_key(&id) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Ok(current);
        }
        let sql = format!(
            "SELECT commitID, pathID FROM {} WHERE commitID IN ({}) \
             AND (isDirect = 1 OR changeType = ?)",
            database.table("repository", "repository_pathchange"),
            phab_db::placeholders(ids.len())
        );
        let changes = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
            .bind(TYPE_CHILD)
            .fetch_all(database.pool())
            .await?;
        let mut paths: HashMap<i64, Vec<i64>> = HashMap::new();
        for row in &changes {
            paths
                .entry(rows::int(row, "commitID")?)
                .or_default()
                .push(rows::int(row, "pathID")?);
        }
        let sql = format!(
            "SELECT childCommitID, parentCommitID FROM {} WHERE childCommitID IN ({}) \
             ORDER BY id ASC",
            database.table("repository", "repository_parents"),
            phab_db::placeholders(ids.len())
        );
        let found = phab_db::bind_all(phab_db::sqlx::query(&sql), ids.iter().copied())
            .fetch_all(database.pool())
            .await?;
        let mut parents: HashMap<i64, Vec<i64>> = HashMap::new();
        for row in &found {
            parents
                .entry(rows::int(row, "childCommitID")?)
                .or_default()
                .push(rows::int(row, "parentCommitID")?);
        }
        for id in ids {
            // Commits without parents rows are left out.
            let Some(parents) = parents.remove(&id) else {
                continue;
            };
            let mut paths = paths.remove(&id).unwrap_or_default();
            paths.sort_unstable();
            current.insert(id, Entry { parents, paths });
        }
        Ok(current)
    }

    /// `getBucketData()`: the bucket from this request, PHP's database
    /// cache or the database; with `current`, rebuilt to fill what it
    /// misses.
    async fn bucket(
        &mut self,
        request: &ApiRequest<'_>,
        bucket: i64,
        current: Option<Bucket>,
    ) -> Result<&Bucket, MethodError> {
        let data = if let Some(current) = current {
            self.rebuild(request, bucket, current).await?
        } else {
            if self.buckets.contains_key(&bucket) {
                return Ok(&self.buckets[&bucket]);
            }
            let cached = cache::read(request, &[cache_key(bucket)])
                .await?
                .pop()
                .flatten()
                .and_then(|data| parse_bucket(&data));
            match cached {
                Some(cached) => cached,
                None => self.rebuild(request, bucket, Bucket::new()).await?,
            }
        };
        self.buckets.insert(bucket, data);
        Ok(&self.buckets[&bucket])
    }

    /// `loadLastModifiedCommitID()`: the commit which last changed the
    /// path at the commit, giving up after `time`.
    pub async fn last_modified_commit_id(
        &mut self,
        request: &ApiRequest<'_>,
        mut commit: i64,
        path: i64,
        time: Duration,
    ) -> Result<Lookup, MethodError> {
        let start = Instant::now();
        let mut seen = HashSet::new();
        let mut iterations: u64 = 0;
        loop {
            let bucket = commit.div_euclid(BUCKET_SIZE);
            let mut did_fill = false;
            let mut entry = self
                .bucket(request, bucket, None)
                .await?
                .get(&commit)
                .cloned();
            if entry.is_none() {
                let current = self.buckets.remove(&bucket).unwrap_or_default();
                entry = self
                    .bucket(request, bucket, Some(current))
                    .await?
                    .get(&commit)
                    .cloned();
                let Some(_) = entry else {
                    return Ok(Lookup::Unknown);
                };
                did_fill = true;
            }
            let Some(entry) = entry else {
                return Ok(Lookup::Unknown);
            };
            // PHP logs the loop.
            if !seen.insert(commit) {
                return Ok(Lookup::Unknown);
            }
            for changed in &entry.paths {
                if *changed > path {
                    break;
                }
                if *changed == path {
                    return Ok(Lookup::Commit(commit));
                }
            }
            match entry.parents.first() {
                Some(&parent) if parent != 0 => {
                    commit = parent;
                    iterations += 1;
                    if (did_fill || iterations % 64 == 0) && start.elapsed() > time {
                        return Ok(Lookup::Unknown);
                    }
                }
                Some(_) => return Ok(Lookup::Root),
                None => return Ok(Lookup::Unknown),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_buckets_like_php() {
        let data = b"a:2:{i:5;a:4:{i:0;i:4;i:1;N;i:2;i:1;i:3;i:7;}i:4;a:1:{i:0;N;}}";
        let bucket = parse_bucket(data).unwrap();
        assert_eq!(
            bucket[&5],
            Entry {
                parents: vec![4],
                paths: vec![1, 7]
            }
        );
        assert_eq!(bucket[&4].parents, Vec::<i64>::new());
        assert_eq!(parse_bucket(b"a:0:{}"), None);
        assert_eq!(parse_bucket(b"garbage"), None);
        assert_eq!(
            cache_key(3),
            "PhabricatorRepositoryGraphCache:4096:2:3".to_owned()
        );
    }
}
