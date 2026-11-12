//! Errors of the authentication code.

use phab_db::DbError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Hash(#[from] phab_core::hash::HashError),
    #[error(transparent)]
    Config(#[from] phab_core::config::ConfigError),
}

impl From<phab_db::sqlx::Error> for AuthError {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Database(error.into())
    }
}
