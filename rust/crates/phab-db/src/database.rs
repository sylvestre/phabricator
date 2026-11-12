//! The connection pool and helpers to build queries.

use phab_core::Config;
use serde_json::{Map, Value};
use sqlx::MySqlPool;
use sqlx::mysql::{MySqlArguments, MySqlConnectOptions, MySqlPoolOptions};
use sqlx::query::Query;

use crate::{DbError, rows};

#[derive(Debug, Clone)]
pub struct Database {
    pool: MySqlPool,
    namespace: String,
}

/// `?, ?, ?` for an `IN (...)` list of `count` values.
pub fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// Binds every value of `values`, in order, to `query`.
pub fn bind_all<'query, T>(
    mut query: Query<'query, sqlx::MySql, MySqlArguments>,
    values: impl IntoIterator<Item = T>,
) -> Query<'query, sqlx::MySql, MySqlArguments>
where
    T: 'query + sqlx::Encode<'query, sqlx::MySql> + sqlx::Type<sqlx::MySql> + Send,
{
    for value in values {
        query = query.bind(value);
    }
    query
}

impl Database {
    /// Connects using the `mysql.*` options, like `PhabricatorDatabaseRef`
    /// does without `cluster.databases`.
    pub async fn connect(config: &Config) -> Result<Self, DbError> {
        let host = config.nonempty_string("mysql.host").unwrap_or("localhost");
        let (host, mut port) = match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => {
                (name, port.parse().ok())
            }
            _ => (host, None),
        };
        if let Some(value) = config.value("mysql.port").filter(|value| !value.is_null()) {
            let text = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            if !text.is_empty() {
                port = Some(text.parse().map_err(|_| DbError::InvalidPort(text))?);
            }
        }
        let mut options = MySqlConnectOptions::new()
            .host(host)
            .port(port.unwrap_or(3306))
            .username(config.string("mysql.user").unwrap_or("root"))
            .charset("utf8mb4");
        if let Some(password) = config.nonempty_string("mysql.pass") {
            options = options.password(password);
        }
        let pool = MySqlPoolOptions::new().connect_with(options).await?;
        let namespace = config
            .nonempty_string("storage.default-namespace")
            .unwrap_or("phabricator")
            .to_owned();
        Ok(Self { pool, namespace })
    }

    pub fn from_pool(pool: MySqlPool, namespace: impl Into<String>) -> Self {
        Self {
            pool,
            namespace: namespace.into(),
        }
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The quoted, database-qualified name of `table` in the database of
    /// `application`, e.g. `` `phabricator_user`.`user` ``.
    pub fn table(&self, application: &str, table: &str) -> String {
        format!("`{}_{application}`.`{table}`", self.namespace)
    }

    /// The `phabricator_config.config_entry` values, which form the
    /// "Database" source of `PhabricatorEnv`.
    pub async fn load_config_entries(&self) -> Result<Map<String, Value>, DbError> {
        let sql = format!(
            "SELECT configKey, value FROM {} WHERE namespace = ? AND isDeleted = 0",
            self.table("config", "config_entry")
        );
        let rows = sqlx::query(&sql)
            .bind("default")
            .fetch_all(&self.pool)
            .await?;
        let mut entries = Map::new();
        for row in rows {
            let key = rows::text(&row, "configKey")?;
            let value = rows::json(&row, "value")?;
            entries.insert(key, value);
        }
        Ok(entries)
    }

    /// Whether the server answers, for health checks.
    pub async fn ping(&self) -> Result<(), DbError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}
