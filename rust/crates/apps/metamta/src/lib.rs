//! `PhabricatorMetaMTAApplication`, the outbound side: what
//! `PhabricatorMetaMTAWorker` does with a queued `metamta_mail` row
//! (`PhabricatorMetaMTAMail::sendNow()`).
//!
//! [`send::plan`] reads everything PHP reads and decides the outcome without
//! writing; [`send::execute`] then delivers the mail and writes the row as
//! PHP's `save()` would. Mail Rust can't handle exactly as PHP does is
//! declined ([`Error::Unsupported`]) before anything is written, so its task
//! stays with PHP.

pub mod actors;
pub mod address;
pub mod engine;
pub mod mail;
pub mod mailer;
pub mod php;
pub mod phpmailer;
pub mod send;
pub mod smtp;

use phab_db::DbError;

/// Why mail could not be planned.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The mail or the configuration needs code which is not ported.
    #[error("not ported: {0}")]
    Unsupported(String),
}

impl Error {
    /// An [`Error::Unsupported`].
    pub(crate) fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
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

/// `PlatformSymbols::getPlatformServerName()`.
pub const SERVER_NAME: &str = "Phabricator";
