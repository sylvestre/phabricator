//! Errors of the database layer.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid 'mysql.port' value '{0}'")]
    InvalidPort(String),
    #[error("column '{column}' has an unexpected type")]
    ColumnType { column: String },
    #[error("column '{column}' does not contain valid JSON: {source}")]
    Json {
        column: String,
        source: serde_json::Error,
    },
    #[error("no edge storage is known for PHIDs of type {0}")]
    UnknownEdgeSource(String),
}
