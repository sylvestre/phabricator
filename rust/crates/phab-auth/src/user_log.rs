//! The account activity log, `PhabricatorUserLog`, as the login and logout
//! workflows write it.

use phab_core::{Phid, php};
use phab_db::Database;
use serde_json::{Map, Value, json};

use crate::AuthError;

/// What a log row records about the request: PHP reads these from the
/// environment (`PhabricatorEnv::getRemoteAddress()`, the `User-Agent`
/// header and `php_uname('n')`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestInfo {
    /// The client address, or empty if unknown.
    pub remote_address: String,
    /// The `User-Agent` header, if sent.
    pub user_agent: Option<String>,
    /// The host name of the server.
    pub hostname: String,
}

/// The `LOGTYPE` of the `PhabricatorUserLogType` classes written here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogType {
    /// `PhabricatorLoginUserLogType`: a full session was issued.
    Login,
    /// `PhabricatorPartialLoginUserLogType`: a partial session was issued.
    PartialLogin,
    /// `PhabricatorFullLoginUserLogType`: a partial session became full.
    FullLogin,
    /// `PhabricatorLogoutUserLogType`.
    Logout,
    /// `PhabricatorLoginFailureUserLogType`: a wrong password.
    LoginFailure,
    /// `PhabricatorEnterHisecUserLogType`: the session went into high
    /// security.
    EnterHisec,
    /// `PhabricatorFailHisecUserLogType`: a multi-factor answer was wrong.
    FailHisec,
}

impl LogType {
    /// The value of the `action` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::PartialLogin => "login-partial",
            Self::FullLogin => "login-full",
            Self::Logout => "logout",
            Self::LoginFailure => "login-fail",
            Self::EnterHisec => "hisec-enter",
            Self::FailHisec => "hisec-fail",
        }
    }
}

/// A row to write, as `PhabricatorUserLog::initializeNewLog()` fills it.
#[derive(Debug, Clone)]
pub struct LogEntry<'a> {
    /// The user acting, if logged in.
    pub actor: Option<&'a Phid>,
    /// The digest of the actor's session key, or of the session the row is
    /// about; never the session token itself.
    pub session: Option<&'a str>,
    /// The user the row is about; empty when there is none.
    pub user_phid: &'a str,
    /// What happened.
    pub action: LogType,
    /// Details set before `save()`, which adds the host and user agent.
    pub details: Map<String, Value>,
}

/// `PhabricatorUserLog::save()`.
pub async fn insert(
    database: &Database,
    entry: LogEntry<'_>,
    request: &RequestInfo,
    now: i64,
) -> Result<(), AuthError> {
    let mut details = entry.details;
    details.insert("host".to_owned(), json!(request.hostname));
    details.insert("user_agent".to_owned(), json!(request.user_agent));
    let sql = format!(
        "INSERT INTO {} (actorPHID, userPHID, action, oldValue, newValue, details, \
           dateCreated, dateModified, remoteAddr, session) \
         VALUES (?, ?, ?, 'null', 'null', ?, ?, ?, ?, ?)",
        database.table("user", "user_log")
    );
    phab_db::sqlx::query(&sql)
        .bind(entry.actor.map(Phid::as_str))
        .bind(entry.user_phid)
        .bind(entry.action.as_str())
        .bind(php::json_encode(&Value::Object(details)))
        .bind(now)
        .bind(now)
        .bind(&request.remote_address)
        .bind(entry.session)
        .execute(database.pool())
        .await?;
    Ok(())
}
