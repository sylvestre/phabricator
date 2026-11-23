//! `PhabricatorApplicationTransactionPublishWorker`: what happens once an
//! editor applied transactions, in the background
//! (`PhabricatorApplicationTransactionEditor::publishTransactions()`): the
//! search indexing task, the feed story with its notifications, and the
//! mail, one `metamta_mail` row per recipient.
//!
//! Ported for Differential revisions (`DifferentialTransactionEditor`).
//! [`publish::plan`] reads everything PHP reads and builds the rows without
//! writing; [`publish::write`] writes them in PHP's order. Whatever Rust
//! can't build exactly as PHP does is [`Error::Unsupported`] at planning
//! time, so the task stays with PHP.

mod body;
mod feed;
mod handles;
mod html;
mod mail;
pub mod publish;
mod state;
mod xaction;

use phab_db::DbError;

/// Why publishing could not be planned.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The task needs code which is not ported.
    #[error("not ported: {0}")]
    Unsupported(String),
}

impl Error {
    /// An [`Error::Unsupported`].
    pub(crate) fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
    }
}

impl From<phab_app_metamta::Error> for Error {
    fn from(error: phab_app_metamta::Error) -> Self {
        match error {
            phab_app_metamta::Error::Db(error) => Self::Db(error),
            phab_app_metamta::Error::Unsupported(reason) => Self::Unsupported(reason),
        }
    }
}

impl From<phab_app_settings::Error> for Error {
    fn from(error: phab_app_settings::Error) -> Self {
        match error {
            phab_app_settings::Error::Db(error) => Self::Db(error),
            phab_app_settings::Error::Unsupported(reason) => Self::unsupported(reason),
        }
    }
}

impl From<phab_app_differential::storage::VisibilityError> for Error {
    fn from(error: phab_app_differential::storage::VisibilityError) -> Self {
        Self::unsupported(format!("a policy check failed: {error}"))
    }
}
