//! Errors of the database layer.

use thiserror::Error;

/// Errors of the database layer.
#[derive(Debug, Error)]
pub enum DbError {
    /// A query or connection failed.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    /// `mysql.port` is not a port number.
    #[error("invalid 'mysql.port' value '{0}'")]
    InvalidPort(String),
    /// A column could not be read as the requested type.
    #[error("column '{column}' has an unexpected type")]
    ColumnType {
        /// Name of the column.
        column: String,
    },
    /// A JSON column holds invalid JSON.
    #[error("column '{column}' does not contain valid JSON: {source}")]
    Json {
        /// Name of the column.
        column: String,
        /// The JSON error.
        source: serde_json::Error,
    },
    /// No edge table is known for the source PHID type.
    #[error("no edge storage is known for PHIDs of type {0}")]
    UnknownEdgeSource(String),
}
