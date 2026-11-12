//! Errors of the authentication code.

use phab_db::DbError;
use thiserror::Error;

/// An error which kept authentication from finishing. Rejected credentials
/// are not errors: they come back in the [`Outcome`](crate::Outcome).
#[derive(Debug, Error)]
pub enum AuthError {
    /// A database query failed.
    #[error(transparent)]
    Database(#[from] DbError),
    /// Computing a token digest failed.
    #[error(transparent)]
    Hash(#[from] phab_core::hash::HashError),
    /// A configuration value could not be read.
    #[error(transparent)]
    Config(#[from] phab_core::config::ConfigError),
}

impl From<phab_db::sqlx::Error> for AuthError {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Database(error.into())
    }
}
