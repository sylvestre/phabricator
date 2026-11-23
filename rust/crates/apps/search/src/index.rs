//! `PhabricatorSearchWorker` and `PhabricatorIndexEngine`: which index
//! extensions an object needs, the lock, and the writes.
//!
//! The extensions of the indexed types are `fulltext`
//! (`PhabricatorFulltextIndexEngineExtension`, whose version is a hash of
//! `cluster.search` and the newest transaction and comment IDs) and, for
//! projects, `project.members`
//! (`PhabricatorProjectsMembershipIndexEngineExtension`, which has no
//! version, so a project is always indexed). An extension whose version
//! matches the `search_indexversion` row is skipped, unless the task's
//! parameters `force` it.

use phab_core::hash::digest_to_length;
use phab_core::{Config, php};
use phab_db::sqlx::pool::PoolConnection;
use phab_db::sqlx::{self, MySqlConnection};
use phab_db::{Database, DbError, rows};
use serde_json::{Value, json};

use crate::object::{self, Kind, Object};
use crate::{Error, ferret};

/// `PhabricatorFulltextIndexEngineExtension::EXTENSIONKEY`.
pub const FULLTEXT: &str = "fulltext";
/// `PhabricatorIndexEngine::saveIndexVersions()`'s marker.
const INDEX_VERSION: &str = "2021-02-16-A";
/// `PhabricatorProjectProjectHasMemberEdgeType`.
const PROJECT_HAS_MEMBER: i64 = 13;
/// `PhabricatorProjectMaterializedMemberEdgeType`.
const PROJECT_MATERIALIZED_MEMBER: i64 = 60;
/// Seconds `PhabricatorSearchWorker` waits for the lock.
const LOCK_WAIT: i64 = 1;
/// Seconds it yields for when another process holds the lock.
pub const LOCKED_YIELD: i64 = 15;
/// Rows per statement when Rust splits a long list.
const CHUNK: usize = 4096;

/// A `PhabricatorSearchWorker` task's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// `documentPHID`.
    pub phid: String,
    /// The object's type.
    pub kind: Kind,
    /// `parameters['force']`: rebuild indexes which are current.
    pub force: bool,
    /// `strict`: report failures (`bin/search index`).
    pub strict: bool,
}

/// Reads the task data (`queueDocumentForIndexing()`'s); other shapes,
/// which make PHP warn or fail, and other object types are unsupported.
pub fn parse_task(data: &Value) -> Result<Task, Error> {
    let Value::Object(data) = data else {
        return Err(Error::unsupported(
            "search task data which is not a dictionary",
        ));
    };
    let Some(Value::String(phid)) = data.get("documentPHID") else {
        return Err(Error::unsupported(
            "search task data without a document PHID",
        ));
    };
    let kind = Kind::of(phid).ok_or_else(|| {
        Error::unsupported("indexing objects other than revisions, users and projects")
    })?;
    // `setParameters(array $parameters)`.
    let force = match data.get("parameters") {
        None | Some(Value::Array(_)) => false,
        Some(Value::Object(parameters)) => parameters.get("force").is_some_and(php::is_truthy),
        Some(_) => {
            return Err(Error::unsupported(
                "search task parameters which are not a list",
            ));
        }
    };
    let strict = data.get("strict").is_some_and(php::is_truthy);
    Ok(Task {
        phid: phid.clone(),
        kind,
        force,
        strict,
    })
}

/// The extensions to run, with the versions to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indexing {
    /// Whether `fulltext` runs.
    pub fulltext: bool,
    /// Whether `project.members` runs.
    pub members: bool,
    /// Every extension's version (`$this->versions`), saved once they ran.
    pub versions: Vec<(&'static str, String)>,
}

/// `cluster.search`, as `PhabricatorEnv::getEnvConfig()` gives it.
fn search_services(config: &Config) -> Value {
    config
        .value("cluster.search")
        .cloned()
        .unwrap_or_else(|| json!([{"type": "mysql", "roles": {"read": true, "write": true}}]))
}

/// Whether a JSON value has a number PHP would encode differently.
fn has_float(value: &Value) -> bool {
    match value {
        Value::Number(number) => !number.is_i64() && !number.is_u64(),
        Value::Array(items) => items.iter().any(has_float),
        Value::Object(entries) => entries.values().any(has_float),
        _ => false,
    }
}

/// `newConfigurationVersion()`: the hash of `cluster.search`. Services of
/// another type than `mysql` (Ferret) are unsupported: PHP fails on them
/// once the Ferret rows are written.
fn configuration_version(config: &Config) -> Result<String, Error> {
    let services = search_services(config);
    let supported = match &services {
        Value::Array(items) => items
            .iter()
            .all(|service| service.get("type").and_then(Value::as_str) == Some("mysql")),
        _ => false,
    };
    if !supported || has_float(&services) {
        return Err(Error::unsupported(
            "cluster.search with other services than mysql",
        ));
    }
    let raw = php::json_encode(&php::round_trip(&json!({"services": services})));
    Ok(phab_core::hash::digest_for_index(raw.as_bytes()))
}

/// The newest ID of a query, as text, or `none`.
async fn newest_id(database: &Database, sql: &str, phid: &str) -> Result<String, DbError> {
    let row = sqlx::query(sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?;
    Ok(match row {
        Some(row) => rows::int(&row, "id")?.to_string(),
        None => "none".to_owned(),
    })
}

/// `PhabricatorFulltextIndexEngineExtension::getIndexVersion()`.
async fn fulltext_version(
    database: &Database,
    config: &Config,
    kind: Kind,
    phid: &str,
) -> Result<String, Error> {
    let configuration = configuration_version(config)?;
    let transactions = database.table(kind.database(), kind.transaction_table());
    let transaction = newest_id(
        database,
        &format!("SELECT id FROM {transactions} WHERE objectPHID = ? ORDER BY id DESC LIMIT 1"),
        phid,
    )
    .await?;
    let comment = match kind.comment_table() {
        None => "none".to_owned(),
        Some(table) => {
            let comments = database.table(kind.database(), table);
            newest_id(
                database,
                &format!(
                    "SELECT c.id FROM {transactions} x JOIN {comments} c \
                     ON x.phid = c.transactionPHID WHERE x.objectPHID = ? \
                     ORDER BY c.id DESC LIMIT 1"
                ),
                phid,
            )
            .await?
        }
    };
    Ok(format!("{configuration}:{transaction}:{comment}"))
}

/// `loadIndexVersions()`.
async fn load_versions(
    database: &Database,
    phid: &str,
    keys: &[&str],
) -> Result<Vec<(String, String)>, DbError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT version, extensionKey FROM {} WHERE objectPHID = ? AND extensionKey IN ({})",
        database.table("search", "search_indexversion"),
        phab_db::placeholders(keys.len())
    );
    phab_db::bind_all(sqlx::query(&sql).bind(phid), keys.iter().copied())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                rows::text(row, "extensionKey")?,
                rows::text(row, "version")?,
            ))
        })
        .collect()
}

/// Loads the object of `task` (`loadObjectForIndexing()`).
pub async fn load_object(
    database: &Database,
    config: &Config,
    task: &Task,
) -> Result<Option<Object>, Error> {
    object::load(database, config, task.kind, &task.phid).await
}

/// `PhabricatorIndexEngine::shouldIndexObject()` for `object`.
pub async fn plan(
    database: &Database,
    config: &Config,
    task: &Task,
    object: &Object,
) -> Result<Indexing, Error> {
    let version = fulltext_version(database, config, task.kind, &object.phid).await?;
    let current = if task.force {
        Vec::new()
    } else {
        load_versions(database, &object.phid, &[FULLTEXT]).await?
    };
    let fulltext = !current
        .iter()
        .any(|(key, stored)| key == FULLTEXT && *stored == version);
    Ok(Indexing {
        fulltext,
        members: task.kind == Kind::Project,
        versions: vec![(FULLTEXT, version)],
    })
}

/// `PhabricatorGlobalLock::newLock('index', array('objectPHID' => ...))`.
pub fn lock_name(namespace: &str, phid: &str) -> String {
    let digest = |text: &str| {
        let bytes = digest_to_length(text.as_bytes(), 20).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    // `phutil_json_encode()` of a PHID, which has nothing to escape.
    let local = format!("index(objectPHID=\"{phid}\")");
    format!("ph:{}:{}", digest(namespace), digest(&local))
}

/// A held `PhabricatorGlobalLock`, on its own connection.
pub struct Lock {
    connection: PoolConnection<sqlx::MySql>,
    name: String,
}

impl Lock {
    /// `lock(1)`; `None` when another process holds it.
    pub async fn acquire(database: &Database, phid: &str) -> Result<Option<Self>, DbError> {
        let name = lock_name(database.namespace(), phid);
        let mut connection = database.pool().acquire().await?;
        let held: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, ?)")
            .bind(&name)
            .bind(LOCK_WAIT)
            .fetch_one(&mut *connection)
            .await?;
        if held != Some(1) {
            return Ok(None);
        }
        Ok(Some(Self { connection, name }))
    }

    /// `unlock()`.
    pub async fn release(mut self) -> Result<(), DbError> {
        sqlx::query("SELECT RELEASE_LOCK(?)")
            .bind(&self.name)
            .execute(&mut *self.connection)
            .await?;
        Ok(())
    }
}

/// `PhabricatorProject::updateDatasourceTokens()`.
async fn write_tokens(
    database: &Database,
    project_id: i64,
    tokens: &[String],
) -> Result<(), DbError> {
    let table = database.table("project", "project_datasourcetoken");
    let mut transaction = database.pool().begin().await?;
    sqlx::query(&format!("DELETE FROM {table} WHERE projectID = ?"))
        .bind(project_id)
        .execute(&mut *transaction)
        .await?;
    for chunk in tokens.chunks(CHUNK) {
        let sql = format!(
            "INSERT INTO {table} (projectID, token) VALUES {}",
            vec!["(?, ?)"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query(&sql);
        for token in chunk {
            query = query.bind(project_id).bind(token.as_str());
        }
        query.execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// `materializeProject()` of a project without parent, subprojects or
/// milestones (the others are not loaded): its materialized members are
/// its members, and it has neither subprojects nor milestones.
async fn materialize_members(database: &Database, object: &Object) -> Result<(), DbError> {
    let edges = database.table("project", "edge");
    let projects = database.table("project", "project");
    let any_milestone = sqlx::query(&format!(
        "SELECT id FROM {projects} WHERE parentProjectPHID = ? AND milestoneNumber IS NOT NULL \
         LIMIT 1"
    ))
    .bind(&object.phid)
    .fetch_optional(database.pool())
    .await?;
    let has_milestones = any_milestone.is_some();

    let mut transaction = database.pool().begin().await?;
    let connection: &mut MySqlConnection = &mut transaction;
    let have: Vec<String> = sqlx::query(&format!(
        "SELECT dst FROM {edges} WHERE src = ? AND type = ?"
    ))
    .bind(&object.phid)
    .bind(PROJECT_MATERIALIZED_MEMBER)
    .fetch_all(&mut *connection)
    .await?
    .iter()
    .map(|row| rows::text(row, "dst"))
    .collect::<Result<_, _>>()?;
    // `ipull($want_rows, null, 'dst')`: the last row of a destination
    // wins, at the place of its first.
    let mut want: Vec<(String, i64, i64)> = Vec::new();
    for row in sqlx::query(&format!(
        "SELECT dst, dateCreated, seq FROM {edges} WHERE src IN (?) AND type = ?"
    ))
    .bind(&object.phid)
    .bind(PROJECT_HAS_MEMBER)
    .fetch_all(&mut *connection)
    .await?
    {
        let edge = (
            rows::text(&row, "dst")?,
            rows::int(&row, "dateCreated")?,
            rows::int(&row, "seq")?,
        );
        match want.iter_mut().find(|known| known.0 == edge.0) {
            Some(known) => *known = edge,
            None => want.push(edge),
        }
    }
    let mut removed: Vec<&String> = Vec::new();
    for dst in &have {
        if !want.iter().any(|(wanted, _, _)| wanted == dst) && !removed.contains(&dst) {
            removed.push(dst);
        }
    }
    let added: Vec<&(String, i64, i64)> = want
        .iter()
        .filter(|(dst, _, _)| !have.contains(dst))
        .collect();

    for chunk in removed.chunks(CHUNK) {
        let sql = format!(
            "DELETE FROM {edges} WHERE src = ? AND type = ? AND dst IN ({})",
            phab_db::placeholders(chunk.len())
        );
        let query = sqlx::query(&sql)
            .bind(&object.phid)
            .bind(PROJECT_MATERIALIZED_MEMBER);
        phab_db::bind_all(query, chunk.iter().map(|dst| dst.as_str()))
            .execute(&mut *connection)
            .await?;
    }
    for chunk in added.chunks(CHUNK) {
        let sql = format!(
            "INSERT IGNORE INTO {edges} (src, type, dst, dateCreated, seq) VALUES {}",
            vec!["(?, ?, ?, ?, ?)"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query(&sql);
        for (dst, date_created, seq) in chunk {
            query = query
                .bind(&object.phid)
                .bind(PROJECT_MATERIALIZED_MEMBER)
                .bind(dst)
                .bind(date_created)
                .bind(seq);
        }
        query.execute(&mut *connection).await?;
    }
    sqlx::query(&format!(
        "UPDATE {projects} SET hasSubprojects = ? WHERE id = ?"
    ))
    .bind(0_i64)
    .bind(object.id)
    .execute(&mut *connection)
    .await?;
    sqlx::query(&format!(
        "UPDATE {projects} SET hasMilestones = ? WHERE id = ?"
    ))
    .bind(i64::from(has_milestones))
    .bind(object.id)
    .execute(&mut *connection)
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// `saveIndexVersions()`.
async fn save_versions(
    database: &Database,
    phid: &str,
    versions: &[(&'static str, String)],
    now: i64,
) -> Result<(), DbError> {
    if versions.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "INSERT INTO {} (objectPHID, extensionKey, version, indexVersion, indexEpoch) \
         VALUES {} ON DUPLICATE KEY UPDATE version = VALUES(version), \
         indexVersion = VALUES(indexVersion), indexEpoch = VALUES(indexEpoch)",
        database.table("search", "search_indexversion"),
        vec!["(?, ?, ?, ?, ?)"; versions.len()].join(", ")
    );
    let mut query = sqlx::query(&sql);
    for (key, version) in versions {
        query = query
            .bind(phid)
            .bind(*key)
            .bind(version.as_str())
            .bind(INDEX_VERSION)
            .bind(now);
    }
    query.execute(database.pool()).await?;
    Ok(())
}

/// What [`prepare`] built for the extensions to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The reloaded object.
    pub object: Object,
    /// The document, if `fulltext` runs.
    pub built: Option<object::Built>,
}

/// Builds what the extensions of `indexing` write for `object`, reading
/// everything first: whatever Rust can't build is unsupported before
/// anything is written.
pub async fn prepare(
    database: &Database,
    config: &Config,
    indexing: &Indexing,
    object: Object,
) -> Result<Prepared, Error> {
    let built = if indexing.fulltext {
        Some(object::build(database, config, &object).await?)
    } else {
        None
    };
    Ok(Prepared { object, built })
}

/// `PhabricatorIndexEngine::indexObject()`: runs the extensions, in PHP's
/// order, then saves the versions.
pub async fn write(
    database: &Database,
    indexing: &Indexing,
    prepared: &Prepared,
    now: i64,
) -> Result<(), DbError> {
    let object = &prepared.object;
    if let Some(built) = &prepared.built {
        if let Some(tokens) = &built.tokens {
            write_tokens(database, object.id, tokens).await?;
        }
        ferret::write(
            database,
            object.kind.ferret(),
            &built.document,
            &built.corpus,
        )
        .await?;
    }
    if indexing.members {
        materialize_members(database, object).await?;
    }
    save_versions(database, &object.phid, &indexing.versions, now).await
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    #[test]
    fn reads_task_data_like_php() {
        let task = parse_task(&json!({
            "documentPHID": "PHID-DREV-x",
            "parameters": {"force": true},
            "strict": false,
        }))
        .unwrap();
        assert_eq!(task.kind, Kind::Revision);
        assert!(task.force);
        assert!(!task.strict);
        let task = parse_task(&json!({"documentPHID": "PHID-USER-x", "parameters": []})).unwrap();
        assert!(!task.force);
        assert!(parse_task(&json!({"documentPHID": "PHID-TASK-x"})).is_err());
        assert!(parse_task(&json!({"documentPHID": "PHID-PROJ-x", "parameters": null})).is_err());
        assert!(parse_task(&json!([])).is_err());
    }

    #[test]
    fn hashes_the_search_configuration() {
        // `phutil_json_encode(array('services' => <default>))`.
        let expected = phab_core::hash::digest_for_index(
            br#"{"services":[{"type":"mysql","roles":{"read":true,"write":true}}]}"#,
        );
        assert_eq!(configuration_version(&Config::default()).unwrap(), expected);

        let mut config = Config::default();
        let mut layer = Map::new();
        layer.insert(
            "cluster.search".to_owned(),
            json!([{"type": "elasticsearch", "hosts": []}]),
        );
        config.push_layer("test", layer);
        assert!(configuration_version(&config).is_err());
    }

    #[test]
    fn names_the_lock_like_php() {
        let name = lock_name("phabricator", "PHID-DREV-x");
        let parts: Vec<&str> = name.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].len(), 20);
        assert_eq!(parts[2].len(), 20);
        assert!(parts[2].starts_with("index(o-"));
    }
}
